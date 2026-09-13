//! Opt-in, metadata-only end-to-end coverage for a real large YouTube channel.
//!
//! Normal test runs exercise the offline harness but never contact YouTube.
//! Run `scripts/test-live-youtube-podcast.sh UC_CHANNEL_ID` explicitly. The
//! optional API key comes only from `YOUTA_LIVE_PODCAST_API_KEY`; this test
//! never reads Youta credentials or uses the user's date/full-description cache.

#![cfg(all(feature = "lan-sharing", feature = "rss"))]

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use feed_rs::model::Feed;
use url::Url;
use youta::lan_share::{
    LanShareServer, PreparedLocalShare, prepare_youtube_podcast_share,
    prepare_youtube_podcast_share_from,
};
use youta::playback::youtube_prewarm::{YouTubePrewarmCancellation, YouTubePrewarmConfig};
use youta::playback::ytdlp::{
    CollectionEntry, ExtractedCollection, YouTubeEpisodeMetadata, YtDlp, YtDlpConfig,
};

/// Maximum duration of the live metadata run, including feed assertions.
const LIVE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Maximum static XML accepted by the loopback-only harness.
const MAX_FEED_BYTES: usize = 32 * 1024 * 1024;

/// Records explicit-key API coverage without exposing credentials or errors.
#[derive(Default)]
struct OfficialBatchStats {
    attempts: Cell<usize>,
    successful: Cell<usize>,
    returned_ids: Cell<usize>,
    failures: Cell<usize>,
}

impl OfficialBatchStats {
    /// Cancels on an API failure so explicit-key runs cannot pass via fallback.
    fn record<T, E>(
        &self,
        result: Result<HashMap<String, T>, E>,
        cancellation: &YouTubePrewarmCancellation,
    ) -> youta::playback::Result<HashMap<String, T>> {
        self.attempts.set(self.attempts.get() + 1);
        match result {
            Ok(metadata) => {
                self.successful.set(self.successful.get() + 1);
                self.returned_ids
                    .set(self.returned_ids.get() + metadata.len());
                Ok(metadata)
            }
            Err(_) => {
                self.failures.set(self.failures.get() + 1);
                cancellation.cancel();
                Err(youta::playback::PlaybackError::Protocol(
                    "explicit live podcast metadata API request failed".to_owned(),
                ))
            }
        }
    }
}

/// Cancels supervised helper processes when the deadline or test scope ends.
struct LiveDeadline {
    cancellation: YouTubePrewarmCancellation,
    finished: mpsc::Sender<()>,
}

impl LiveDeadline {
    /// Starts a watchdog whose channel wakes immediately on success or panic.
    fn start(timeout: Duration) -> Self {
        let cancellation = YouTubePrewarmCancellation::new();
        let worker_cancellation = cancellation.clone();
        let (finished, receiver) = mpsc::channel();
        // No blocking join is needed during unwinding: the worker owns only
        // the token and exits as soon as this sender signals or disconnects.
        std::thread::spawn(move || {
            if matches!(
                receiver.recv_timeout(timeout),
                Err(RecvTimeoutError::Timeout)
            ) {
                worker_cancellation.cancel();
            }
        });
        Self {
            cancellation,
            finished,
        }
    }

    /// Reports an overall timeout separately from provider-specific failures.
    fn assert_active(&self) {
        assert!(
            !self.cancellation.is_cancelled(),
            "live podcast test exceeded its deadline"
        );
    }
}

impl Drop for LiveDeadline {
    /// Stops the watchdog and any remaining supervised metadata work.
    fn drop(&mut self) {
        self.cancellation.cancel();
        let _ = self.finished.send(());
    }
}

/// Checks IDs before allowing a script or environment value into live requests.
fn valid_channel_id(value: &str) -> bool {
    value.len() == 24
        && value.starts_with("UC")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

/// Requires a genuinely large catalogue without fixing its changing live count.
fn minimum_episodes(value: &str) -> Option<usize> {
    let minimum = value.parse().ok()?;
    (500..=10_000).contains(&minimum).then_some(minimum)
}

/// Gives feed preparation a nonexistent helper so XML checks cannot resolve media.
fn xml_only_config(directory: &Path) -> YouTubePrewarmConfig {
    YouTubePrewarmConfig {
        executable: directory.join("nonexistent-yt-dlp-must-not-run"),
        ..YouTubePrewarmConfig::default()
    }
}

/// Recognizes provider-classified Shorts by their canonical URL, not duration.
fn is_short(entry: &CollectionEntry) -> bool {
    entry.webpage_url.as_ref().is_some_and(|url| {
        url.domain()
            .is_some_and(|host| host == "youtube.com" || host.ends_with(".youtube.com"))
            && url.path_segments().and_then(|mut segments| segments.next()) == Some("shorts")
    })
}

/// Fetches only the static XML over loopback, bypassing LAN routing and proxies.
fn fetch_feed(advertised: &Url, deadline: &LiveDeadline) -> Feed {
    let port = advertised.port().expect("ephemeral LAN server port");
    let socket = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&socket, Duration::from_secs(5))
        .expect("connect to feed over loopback");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /feed.xml HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("request feed XML only");
    let mut response = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        deadline.assert_active();
        let count = stream.read(&mut buffer).expect("read static feed response");
        if count == 0 {
            break;
        }
        assert!(
            response.len() + count <= MAX_FEED_BYTES,
            "feed exceeds test byte limit"
        );
        response.extend_from_slice(&buffer[..count]);
    }
    let split = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("HTTP header terminator");
    let headers = std::str::from_utf8(&response[..split]).expect("ASCII response headers");
    assert!(
        headers.starts_with("HTTP/1.1 200 "),
        "feed request must succeed"
    );
    let body = &response[split + 4..];
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .expect("static feed must advertise its exact length");
    assert_eq!(content_length, body.len(), "feed Content-Length");
    feed_rs::parser::parse(body).expect("parse served RSS with production feed parser")
}

/// Checks a advertised resource URL without fetching artwork or episode media.
fn assert_local_route(value: &str, advertised: &Url, expected_path: &str) {
    let resource = Url::parse(value).expect("absolute resource URL");
    assert_eq!(
        resource.origin(),
        advertised.origin(),
        "resource must use this LAN server"
    );
    assert_eq!(resource.path(), expected_path);
    assert!(resource.username().is_empty() && resource.password().is_none());
    assert!(resource.query().is_none() && resource.fragment().is_none());
}

/// Serves and validates every episode's identity, full text, date, artwork, and enclosure.
fn assert_served_feed(
    prepared: PreparedLocalShare,
    expected: &[CollectionEntry],
    episode_artwork_offset: usize,
    deadline: &LiveDeadline,
) {
    assert_eq!(prepared.item_count(), expected.len());
    let server = LanShareServer::start(prepared).expect("start isolated feed server");
    let advertised = Url::parse(server.url()).expect("advertised feed URL");
    assert_eq!(advertised.path(), "/feed.xml");
    let feed = fetch_feed(&advertised, deadline);
    assert_eq!(
        feed.entries.len(),
        expected.len(),
        "RSS must not truncate the catalogue"
    );
    assert!(
        feed.title
            .as_ref()
            .is_some_and(|title| !title.content.is_empty())
    );
    let artwork = feed.logo.as_ref().expect("podcast-level artwork");
    assert_local_route(&artwork.uri, &advertised, "/artwork/0");
    for (index, (episode, original)) in feed.entries.iter().zip(expected).enumerate() {
        assert_eq!(
            episode.id,
            format!("urn:youta:youtube:{}", original.id),
            "episode order at {index}"
        );
        assert_eq!(
            episode.published.map(|date| date.timestamp()),
            original.published_at,
            "original publication date of {}",
            original.id
        );
        assert!(episode.published.is_some(), "publication date is required");
        if let Some(description) = &original.description {
            assert_eq!(
                episode
                    .summary
                    .as_ref()
                    .map(|summary| summary.content.as_str()),
                Some(description.as_str()),
                "full description must survive the served RSS body for {}",
                original.id,
            );
        }
        let contents = episode
            .media
            .iter()
            .flat_map(|media| &media.content)
            .collect::<Vec<_>>();
        assert_eq!(contents.len(), 1, "one audio enclosure per episode");
        assert_eq!(
            contents[0]
                .content_type
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("audio/webm")
        );
        let encoded =
            percent_encoding::utf8_percent_encode(&original.id, percent_encoding::NON_ALPHANUMERIC);
        assert_local_route(
            contents[0].url.as_ref().expect("enclosure URL").as_str(),
            &advertised,
            &format!("/media/{index}/{encoded}.webm"),
        );
        let images = episode
            .media
            .iter()
            .flat_map(|media| &media.thumbnails)
            .collect::<Vec<_>>();
        assert!(!images.is_empty(), "episode artwork for {}", original.id);
        assert_local_route(
            &images[0].image.uri,
            &advertised,
            &format!("/artwork/{}", index + episode_artwork_offset),
        );
    }
    // No enclosure or artwork endpoint is requested. Dropping the server
    // closes its listener; its resolver is disabled by xml_only_config.
}

/// Chooses an interior boundary, preferring a Short to test filtering order.
fn cutoff_index(entries: &[CollectionEntry]) -> usize {
    let midpoint = entries.len() / 2;
    (1..entries.len() - 1)
        .filter(|&index| {
            is_short(&entries[index]) && entries[..index].iter().any(|entry| !is_short(entry))
        })
        .min_by_key(|&index| index.abs_diff(midpoint))
        .unwrap_or(midpoint)
}

/// Exercises complete and selected-plus-newer feeds using the same catalogue.
fn assert_feed_variants(
    collection: &ExtractedCollection,
    directory: &Path,
    deadline: &LiveDeadline,
) {
    let all = collection.entries.iter().rev().cloned().collect::<Vec<_>>();
    let prepared =
        prepare_youtube_podcast_share(collection.clone(), xml_only_config(directory), false)
            .expect("prepare complete channel podcast");
    let episode_artwork_offset = usize::from(collection.thumbnail_url.is_some());
    assert_served_feed(prepared, &all, episode_artwork_offset, deadline);
    let boundary = cutoff_index(&collection.entries);
    let selected_id = &collection.entries[boundary].id;
    for skip_shorts in [false, true] {
        let expected = collection.entries[..=boundary]
            .iter()
            .rev()
            .filter(|entry| !skip_shorts || !is_short(entry))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            !expected.is_empty() && expected.len() < all.len(),
            "cutoff must remove older episodes"
        );
        if !skip_shorts {
            assert_eq!(&expected[0].id, selected_id);
        }
        let prepared = prepare_youtube_podcast_share_from(
            collection.clone(),
            xml_only_config(directory),
            selected_id,
            skip_shorts,
        )
        .expect("prepare selected-plus-newer podcast");
        assert_served_feed(prepared, &expected, episode_artwork_offset, deadline);
    }
}

/// Enumerates a real large channel, verifies its RSS, and proves offline cache reuse.
#[test]
#[ignore = "requires YouTube and yt-dlp; run scripts/test-live-youtube-podcast.sh UC_CHANNEL_ID"]
fn youtube_large_channel_podcast_dates_and_cache() {
    assert_eq!(
        std::env::var("YOUTA_RUN_LIVE_YOUTUBE_PODCAST_TEST").as_deref(),
        Ok("1"),
        "set YOUTA_RUN_LIVE_YOUTUBE_PODCAST_TEST=1 to explicitly opt in"
    );
    let channel_id =
        std::env::var("YOUTA_LIVE_PODCAST_CHANNEL_ID").expect("set YOUTA_LIVE_PODCAST_CHANNEL_ID");
    assert!(
        valid_channel_id(&channel_id),
        "expected a canonical 24-character UC channel ID"
    );
    let minimum = minimum_episodes(
        &std::env::var("YOUTA_LIVE_PODCAST_MIN_EPISODES").unwrap_or_else(|_| "500".to_owned()),
    )
    .expect("YOUTA_LIVE_PODCAST_MIN_EPISODES must be between 500 and 10000");
    let temporary = tempfile::tempdir().expect("isolated metadata cache");
    let cache = temporary.path().join("podcast-metadata");
    let deadline = LiveDeadline::start(LIVE_TIMEOUT);
    let ytdlp = YtDlp::new(YtDlpConfig {
        executable: std::env::var_os("YOUTA_TEST_YT_DLP")
            .map_or_else(|| PathBuf::from("yt-dlp"), PathBuf::from),
        ..YtDlpConfig::default()
    });
    let started = Instant::now();
    let mut collection = ytdlp
        .youtube_channel_collection_cancellable(&channel_id, u16::MAX, true, &deadline.cancellation)
        .expect("enumerate the complete unified channel catalogue with Shorts classification");
    let enumeration_time = started.elapsed();
    deadline.assert_active();
    let unique = collection
        .entries
        .iter()
        .map(|entry| &entry.id)
        .collect::<HashSet<_>>();
    assert_eq!(
        unique.len(),
        collection.entries.len(),
        "catalogue must not repeat video IDs"
    );
    assert!(
        unique.len() >= minimum,
        "large-channel regression: only {} episodes; expected at least {minimum}",
        unique.len()
    );
    let shorts = collection
        .entries
        .iter()
        .filter(|entry| is_short(entry))
        .count();
    println!(
        "enumeration: {:?}; episodes: {}; Shorts: {shorts}",
        enumeration_time,
        collection.entries.len()
    );

    #[cfg(feature = "youtube-official")]
    let official = std::env::var("YOUTA_LIVE_PODCAST_API_KEY").ok().map(|key| {
        youta::providers::youtube_official::YouTubeOfficialProvider::with_options(
            key,
            Duration::from_secs(8),
            4 * 1024 * 1024,
        )
        .unwrap_or_else(|_| panic!("could not initialize the explicitly configured API client"))
    });
    #[cfg(not(feature = "youtube-official"))]
    assert!(
        std::env::var_os("YOUTA_LIVE_PODCAST_API_KEY").is_none(),
        "explicit API key requires the youtube-official build feature"
    );
    let batches = OfficialBatchStats::default();
    let cold_started = Instant::now();
    let metadata_result = ytdlp.populate_youtube_podcast_metadata_with_batch(
        &mut collection.entries,
        &cache,
        &deadline.cancellation,
        |ids| {
            assert!(
                !ids.is_empty() && ids.len() <= 50,
                "full episode metadata must be batched safely"
            );
            #[cfg(feature = "youtube-official")]
            if let Some(provider) = &official {
                let metadata = provider.podcast_metadata(ids).map(|items| {
                    items
                        .into_iter()
                        .map(|(id, (published_at, description))| {
                            (
                                id,
                                YouTubeEpisodeMetadata {
                                    published_at: Some(published_at),
                                    description: Some(description),
                                },
                            )
                        })
                        .collect()
                });
                return batches.record(metadata, &deadline.cancellation);
            }
            Ok(HashMap::new())
        },
    );
    assert_eq!(
        batches.failures.get(),
        0,
        "explicit API-key requests failed; keyless fallback is not accepted in API mode"
    );
    metadata_result
        .expect("populate all original dates and full descriptions using an isolated cold cache");
    deadline.assert_active();
    println!(
        "cold metadata: {:?}; official API batch calls: {}; successful batches: {}; returned IDs: {}",
        cold_started.elapsed(),
        batches.attempts.get(),
        batches.successful.get(),
        batches.returned_ids.get()
    );
    assert!(
        collection
            .entries
            .iter()
            .all(|entry| entry.published_at.is_some() && entry.description.is_some()),
        "every episode needs its original date and a verified full description"
    );
    assert_feed_variants(&collection, temporary.path(), &deadline);

    let mut warm = collection.clone();
    for entry in &mut warm.entries {
        entry.published_at = None;
        entry.description = None;
    }
    let offline = YtDlp::new(YtDlpConfig {
        executable: temporary.path().join("nonexistent-cache-miss-must-fail"),
        ..YtDlpConfig::default()
    });
    let warm_started = Instant::now();
    // Every cache miss reaches the batch closure before any network fallback.
    // A panic here proves the warm path needs neither HTTP nor the extractor.
    offline
        .populate_youtube_podcast_metadata_with_batch(
            &mut warm.entries,
            &cache,
            &deadline.cancellation,
            |_| panic!("warm full metadata cache must never request network or helper work"),
        )
        .expect("warm cache must restore every date and full description without network or an executable");
    println!(
        "warm cache: {:?}; episodes: {}",
        warm_started.elapsed(),
        warm.entries.len()
    );
    assert_eq!(
        warm, collection,
        "warm cache must preserve exact dates, full descriptions, and order"
    );
    assert_feed_variants(&warm, temporary.path(), &deadline);
    deadline.assert_active();
    println!(
        "total: {:?}; XML-only validation completed without media downloads",
        started.elapsed()
    );
}

/// Builds a deterministic newest-first catalogue to verify the live-test harness.
fn fixture_collection() -> ExtractedCollection {
    let entries = ["newest00001", "shorts00001", "oldest00001"]
        .into_iter()
        .enumerate()
        .map(|(index, id)| CollectionEntry {
            description: Some(format!(
                "Episode {index}\n{}\nDESCRIPTION END {index}",
                "Полное описание 🎵 & <details>\n".repeat(600),
            )),
            id: id.to_owned(),
            title: format!("Episode {index}"),
            webpage_url: Some(
                Url::parse(&if index == 1 {
                    format!("https://www.youtube.com/shorts/{id}")
                } else {
                    format!("https://www.youtube.com/watch?v={id}")
                })
                .unwrap(),
            ),
            duration_seconds: Some(60),
            thumbnail_url: Some(
                Url::parse(&format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")).unwrap(),
            ),
            published_at: Some(1_700_000_000 - i64::try_from(index).unwrap() * 86_400),
        })
        .collect();
    ExtractedCollection {
        description: Some("Offline harness full channel description".to_owned()),
        id: "UC0000000000000000000000".to_owned(),
        title: "Offline harness fixture".to_owned(),
        extractor: Some("youtube:tab".to_owned()),
        thumbnail_url: Some(Url::parse("https://example.invalid/channel.jpg").unwrap()),
        entries,
    }
}

/// Uses loopback and mock metadata to check the harness without contacting YouTube.
#[test]
fn podcast_xml_harness_checks_order_cutoff_dates_and_artwork_offline() {
    let temporary = tempfile::tempdir().unwrap();
    let deadline = LiveDeadline::start(Duration::from_secs(30));
    let mut collection = fixture_collection();
    assert_feed_variants(&collection, temporary.path(), &deadline);
    collection.thumbnail_url = None;
    assert_feed_variants(&collection, temporary.path(), &deadline);
}

/// Allows a verified empty source description without inventing fallback text.
#[test]
fn podcast_xml_harness_accepts_known_empty_descriptions_offline() {
    let temporary = tempfile::tempdir().unwrap();
    let deadline = LiveDeadline::start(Duration::from_secs(30));
    let mut collection = fixture_collection();
    collection.description = Some(String::new());
    collection.entries[1].description = Some(String::new());
    assert_feed_variants(&collection, temporary.path(), &deadline);
}

/// Ensures API-mode errors cannot silently fall back or print sensitive details.
#[test]
fn explicit_api_batch_failures_cancel_without_exposing_error_details() {
    let batches = OfficialBatchStats::default();
    let cancellation = YouTubePrewarmCancellation::new();
    let metadata = HashMap::from([(
        "newest00001".to_owned(),
        YouTubeEpisodeMetadata {
            published_at: Some(1_700_000_000),
            description: Some("Complete episode description".to_owned()),
        },
    )]);
    assert_eq!(
        batches
            .record::<_, &str>(Ok(metadata.clone()), &cancellation)
            .unwrap(),
        metadata
    );
    let error = batches
        .record::<YouTubeEpisodeMetadata, _>(Err("private test credential"), &cancellation)
        .unwrap_err();
    assert!(cancellation.is_cancelled());
    assert_eq!(batches.attempts.get(), 2);
    assert_eq!(batches.successful.get(), 1);
    assert_eq!(batches.returned_ids.get(), 1);
    assert_eq!(batches.failures.get(), 1);
    assert!(!error.to_string().contains("private test credential"));
}

/// Keeps accidental live invocations and too-small channel fixtures out of the test.
#[test]
fn live_channel_and_size_guards_are_strict() {
    assert!(valid_channel_id("UCGebHjxFlDL8kRNejhoDQRg"));
    for invalid in [
        "",
        "@irinapodzorova",
        "https://www.youtube.com/@irinapodzorova",
        "UC../not-a-channel-value",
    ] {
        assert!(!valid_channel_id(invalid));
    }
    assert_eq!(minimum_episodes("500"), Some(500));
    assert_eq!(minimum_episodes("1000"), Some(1000));
    for invalid in ["0", "499", "10001", "", "NaN"] {
        assert_eq!(minimum_episodes(invalid), None);
    }
}

/// Verifies the shell wrapper's guards without ever reaching its cargo invocation.
#[cfg(unix)]
#[test]
fn live_podcast_script_rejects_missing_or_invalid_arguments() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/test-live-youtube-podcast.sh");
    for (arguments, expected) in [
        (vec![], 2),
        (vec!["not-a-channel"], 2),
        (vec!["UCGebHjxFlDL8kRNejhoDQRg", "extra"], 2),
        (vec!["--help"], 0),
    ] {
        let output = std::process::Command::new("bash")
            .arg(&script)
            .args(arguments)
            .env_remove("YOUTA_LIVE_PODCAST_CHANNEL_ID")
            .env_remove("YOUTA_LIVE_PODCAST_API_KEY")
            .env_remove("YOUTA_LIVE_PODCAST_MIN_EPISODES")
            .output()
            .expect("execute wrapper guard");
        assert_eq!(output.status.code(), Some(expected));
    }
    let output = std::process::Command::new("bash")
        .arg(script)
        .arg("UCGebHjxFlDL8kRNejhoDQRg")
        .env("YOUTA_LIVE_PODCAST_MIN_EPISODES", "499")
        .env_remove("YOUTA_LIVE_PODCAST_API_KEY")
        .output()
        .expect("execute size guard");
    assert_eq!(output.status.code(), Some(2));
}
