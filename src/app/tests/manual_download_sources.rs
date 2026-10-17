//! Source and lifecycle regressions for durable downloads; all transfers use fake children.

use super::*;
use crate::download_queue::DownloadQueueState;
#[cfg(not(feature = "archive-org"))]
use crate::download_queue::{
    DownloadQueue, DownloadQueueEntry, DownloadSource, QueuedDownloadFormat,
};

/// Keeps one supervised fake transfer active while later work is inspected.
fn source_test_controller() -> (
    AppController,
    Arc<Mutex<Vec<DownloadRequest>>>,
    tempfile::TempDir,
) {
    let directory = crate::test_support::canonical_tempdir("manual download source regression");
    let config = Config::for_dir(directory.path().join("config"));
    let process = MockRunningDownload {
        progress: Some(Cursor::new(Vec::new())),
        errors: Some(Cursor::new(Vec::new())),
        exits: VecDeque::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (controller, requests, _) = controller_with_mock_download(config, process);
    (controller, requests, directory)
}

/// A selected Funkwhale track is downloaded from its API, never its HTML embed.
#[cfg(feature = "funkwhale")]
#[test]
fn funkwhale_manual_download_uses_native_audio_and_keeps_only_stable_intent() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let _guard = crate::app::funkwhale_download::test_guard();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/prefix/", listener.local_addr().unwrap());
    let page = format!("{base}library/tracks/27");
    let audio = b"ID3\x03\0fixture original encoded audio TAG unchanged";
    let requested = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&requested);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopped);
    let server = thread::spawn(move || {
        while !stop.load(AtomicOrdering::Acquire) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("fixture accept: {error}"),
            };
            // Reproduce Windows' inherited mode even when accept defaults to blocking.
            stream.set_nonblocking(true).unwrap();
            crate::test_support::configure_http_fixture_stream(&stream, Duration::from_secs(2));
            #[cfg(all(unix, feature = "acoustid"))]
            assert!(
                !rustix::fs::fcntl_getfl(&stream)
                    .unwrap()
                    .contains(rustix::fs::OFlags::NONBLOCK),
                "manual download fixture must wait for HTTP request bytes"
            );
            let mut input = [0; 8192];
            let count = stream.read(&mut input).unwrap();
            let request = String::from_utf8_lossy(&input[..count]);
            let path = request.split_whitespace().nth(1).unwrap().to_owned();
            observed.lock().unwrap().push(path.clone());
            let (content_type, body) = if path == "/prefix/api/v1/tracks/27/" {
                ("application/json", serde_json::to_vec(&serde_json::json!({
                    "id": 27, "title": "Original track", "is_playable": true,
                    "listen_url": "/prefix/api/v1/listen/track/?signature=temporary-private",
                    "uploads": [{"uuid": "12345678-1234-1234-1234-123456789abc", "size": audio.len(), "extension": "mp3", "mimetype": "audio/mpeg", "duration": 123}]
                })).unwrap())
            } else if path.starts_with("/prefix/api/v1/listen/track/?") {
                ("audio/mpeg", audio.to_vec())
            } else {
                panic!("The native downloader must not fetch the HTML page: {path}");
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    let (mut controller, generic_requests, _directory) = source_test_controller();
    controller.config.subscriptions.audio_format = "original".to_owned();
    controller.config.providers.funkwhale_instance_url =
        Some(url::Url::parse("https://later-preference.example/").unwrap());
    let mut source = resolved_source_fixture(SourceKind::Funkwhale, &page, &page);
    source.external_id = page.clone();
    source.title = "Native track".to_owned();
    controller.resolved_direct = Some(source);
    controller.dispatch(UiAction::Download);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        controller.poll_download_at(Instant::now());
        if controller.store.download_queue().unwrap().entries[0].state
            == DownloadQueueState::Completed
        {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    stopped.store(true, AtomicOrdering::Release);
    server.join().unwrap();
    assert!(
        generic_requests.lock().unwrap().is_empty(),
        "Funkwhale HTML cannot reach yt-dlp"
    );
    let queue = controller.store.download_queue().unwrap();
    assert_eq!(
        queue.entries[0].state,
        DownloadQueueState::Completed,
        "{}",
        controller.view.status_line
    );
    let output = controller
        .config
        .downloads_dir()
        .join(queue.entries[0].completed_path.as_ref().unwrap());
    assert_eq!(std::fs::read(&output).unwrap(), audio);
    assert_eq!(output.extension().unwrap(), "mp3");
    assert!(
        output
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Native track [27]")
    );
    assert_eq!(queue.entries[0].source.download_url.as_str(), page);
    assert_eq!(queue.entries[0].source.webpage_url.as_str(), page);
    assert!(
        !serde_json::to_string(&queue)
            .unwrap()
            .contains("temporary-private")
    );
    let requested = requested.lock().unwrap();
    assert_eq!(
        requested.len(),
        2,
        "One fresh track lookup and one audio transfer"
    );
    assert!(requested[1].contains("signature=temporary-private"));
    let download = controller.view.download.as_ref().unwrap();
    assert_eq!(download.downloaded_bytes, audio.len() as u64);
    assert_eq!(download.completed_files, 1);
}

/// Original-first playback leaves an explicitly requested MP3 download independent.
#[cfg(feature = "archive-org")]
#[test]
fn archive_original_playback_preserves_the_explicit_mp3_download_choice() {
    use crate::providers::archive_org::{ArchiveOrgClient, ArchiveOrgTransport};

    /// Returns only fixture metadata; no media or external helper is accessed.
    struct MetadataTransport(Vec<u8>);

    impl ArchiveOrgTransport for MetadataTransport {
        fn fetch(
            &self,
            url: &url::Url,
            _: usize,
        ) -> Result<Vec<u8>, crate::providers::ProviderError> {
            if url.as_str() != "https://archive.org/metadata/fixture" {
                return Err(crate::providers::ProviderError::HttpStatus(404));
            }
            Ok(self.0.clone())
        }
    }

    for (filename, format) in [
        ("original.flac", "Flac"),
        ("original.asf", "Windows Media Audio"),
        ("original.mp4", "MPEG4"),
    ] {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "metadata": {"identifier": "fixture", "mediatype": "audio", "title": "Original recording"},
            "files": [
                {"name": filename, "source": "original", "format": format},
                {"name": "generated.mp3", "source": "derivative", "original": filename, "format": "VBR MP3"},
                {"name": "generated.ogg", "source": "derivative", "original": filename, "format": "Ogg Vorbis"},
                {"name": "original.png", "source": "derivative", "original": filename, "format": "PNG", "size": 11254}
            ]
        })).unwrap();
        let client = ArchiveOrgClient::with_transport(Arc::new(MetadataTransport(metadata)));
        let details = client
            .item_details("fixture")
            .expect("original audio family");
        let track = &details.tracks[0];
        let queue = crate::app::archive_org::queue_item(&details.item, track);
        let original_url = format!("https://archive.org/download/fixture/{filename}");
        assert_eq!(queue.playback_location, original_url);
        assert_eq!(queue.media.webpage_url.as_str(), original_url);
        assert_eq!(queue.media.kind, crate::domain::MediaKind::Audio);
        assert_eq!(
            queue.media.id,
            MediaId::new(SourceKind::ArchiveOrg, &original_url)
        );
        assert!(crate::app::archive_org::is_direct_audio_url(
            &queue.media.webpage_url
        ));
        assert_eq!(queue.media.thumbnail_url, track.waveform_url);

        let (mut controller, requests, _directory) = source_test_controller();
        controller.config.downloads.archive_format =
            crate::config::ArchiveDownloadPreference::ArchiveMp3;
        controller.choose_archive_download(queue, track.download_variants.clone());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].source_url.as_str(),
            "https://archive.org/download/fixture/generated.mp3"
        );
        assert_eq!(requests[0].format, DownloadFormat::ExactFile);
    }
}

/// Exercises the real resolved-media adapter with separate stable page and media URLs.
fn resolved_source_fixture(
    source: SourceKind,
    webpage: &str,
    playback: &str,
) -> ResolvedDirectMedia {
    ResolvedDirectMedia {
        external_id: format!("fixture-{}", source.as_str()),
        title: format!("{} fixture", source.as_str()),
        source,
        row_subtitle: String::new(),
        description: String::new(),
        license: String::new(),
        published: None,
        artwork_url: None,
        duration_seconds: Some(120),
        playback_url: Some(url::Url::parse(playback).unwrap()),
        webpage_url: Some(url::Url::parse(webpage).unwrap()),
        status_line: "Fixture resolved".to_owned(),
    }
}

/// Provider identities stay distinct even when unrelated sources share a public page.
#[test]
fn download_marks_capture_mixed_direct_sources_without_retaining_transient_streams() {
    let (mut controller, requests, _directory) = source_test_controller();
    let webpage = "https://media.example.test/fixture";
    let enclosure = "https://media.example.test/fixture.opus";
    let providers = [
        SourceKind::Rss,
        SourceKind::ApplePodcasts,
        SourceKind::WikimediaCommons,
        SourceKind::Bandcamp,
        SourceKind::Odysee,
        SourceKind::Rumble,
        SourceKind::Bilibili,
        SourceKind::PeerTube,
        SourceKind::Vimeo,
        SourceKind::RuTube,
        SourceKind::SoundCloud,
        SourceKind::Jamendo,
        SourceKind::SoundStream,
        SourceKind::LitRes,
        SourceKind::BbcRadio,
        SourceKind::ModArchive,
        SourceKind::GenericYtDlp,
        SourceKind::Vk,
        SourceKind::Telegram,
        SourceKind::Radio,
        SourceKind::RemoteFiles,
        SourceKind::Other("fixture-plugin".to_owned()),
    ];
    for provider in &providers {
        let uses_enclosure = matches!(
            provider,
            SourceKind::Rss | SourceKind::ApplePodcasts | SourceKind::Radio
        );
        let playback = if uses_enclosure {
            enclosure
        } else {
            "https://cdn.example.test/audio.opus?token=transient-secret"
        };
        let resolved = resolved_source_fixture(provider.clone(), webpage, playback);
        let expected_id = MediaId::new(provider.clone(), &resolved.external_id);
        controller.resolved_direct = Some(resolved);
        controller.dispatch(UiAction::ToggleDownloadMark);
        let source = controller
            .manual_downloads
            .marks
            .last()
            .expect("marked remote source");
        assert_eq!(
            source.media_id, expected_id,
            "{}",
            controller.view.status_line
        );
        assert_eq!(
            source.download_url.as_str(),
            if uses_enclosure { enclosure } else { webpage }
        );
        assert!(
            !serde_json::to_string(source)
                .unwrap()
                .contains("transient-secret")
        );
    }
    assert_eq!(controller.manual_downloads.marks.len(), providers.len());
    assert!(
        requests.lock().unwrap().is_empty(),
        "marking must never start a transfer"
    );
    controller.dispatch(UiAction::Download);
    let saved = controller.store.download_queue().unwrap();
    assert_eq!(saved.entries.len(), providers.len());
    for (entry, provider) in saved.entries.iter().zip(&providers) {
        assert_eq!(&entry.source.media_id.source, provider);
    }
    assert!(
        !serde_json::to_string(&saved)
            .unwrap()
            .contains("transient-secret")
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "mixed providers still use one worker"
    );
}

/// Archive marks identify the selected original file, not its containing collection.
#[test]
fn download_archive_mark_retains_the_exact_original_file_identity() {
    let (mut controller, requests, _directory) = source_test_controller();
    let address = "https://archive.org/download/fixture/chapter.flac";
    let mut media = resolved_source_fixture(SourceKind::ArchiveOrg, address, address);
    media.external_id = address.to_owned();
    controller.resolved_direct = Some(media);
    controller.dispatch(UiAction::ToggleDownloadMark);
    let source = controller
        .manual_downloads
        .marks
        .first()
        .expect("Archive exact file mark");
    assert_eq!(
        source.media_id,
        MediaId::new(SourceKind::ArchiveOrg, address)
    );
    assert_eq!(source.download_url.as_str(), address);
    assert_eq!(source.webpage_url.as_str(), address);
    assert!(requests.lock().unwrap().is_empty());
}

/// A real LibriVox chapter downloads its stable audio URL while retaining its book page.
#[cfg(feature = "librivox")]
#[test]
fn download_librivox_capture_keeps_chapter_audio_distinct_from_book_page() {
    let (mut controller, requests, _directory) = source_test_controller();
    let book = librivox_book_fixture();
    let expected = queue_item_from_librivox_section(&book, &book.sections[1]);
    controller.view.screen = Screen::LibriVox;
    controller.librivox_route = LibrivoxRoute::Book;
    controller.active_librivox_book = Some(book.clone());
    controller.view.selected = 1;
    controller.dispatch(UiAction::ToggleDownloadMark);
    let source = controller
        .manual_downloads
        .marks
        .first()
        .expect("LibriVox chapter is markable");
    assert_eq!(source.media_id, expected.media.id);
    assert_eq!(source.webpage_url, book.webpage_url);
    assert_eq!(source.download_url.as_str(), expected.playback_location);
    controller.dispatch(UiAction::Download);
    assert_eq!(
        requests.lock().unwrap()[0].source_url.as_str(),
        expected.playback_location
    );
    assert_eq!(
        controller.store.download_queue().unwrap().entries[0].state,
        DownloadQueueState::Running
    );
}

/// Native tracks contribute the same stable marks as the other source adapters.
#[cfg(feature = "yandex-music")]
#[test]
fn download_yandex_mark_captures_native_track_without_resolving_credentials() {
    let (mut controller, requests, _directory) = source_test_controller();
    let track = yandex_music_track_fixture();
    controller.view.screen = Screen::YandexMusic;
    controller.yandex_music_rows = vec![YandexMusicRow::Track(Box::new(track.clone()))];
    controller.view.selected = 0;
    controller.dispatch(UiAction::ToggleDownloadMark);
    let source = controller
        .manual_downloads
        .marks
        .first()
        .expect("native track mark");
    assert_eq!(
        source.media_id,
        MediaId::new(SourceKind::YandexMusic, &track.id)
    );
    assert_eq!(source.webpage_url, track.webpage_url);
    assert_eq!(source.download_url, track.webpage_url);
    assert!(requests.lock().unwrap().is_empty());
    assert!(controller.yandex_music_download_thread.is_none());
}

/// Cached playback context must not replace the canonical persisted YouTube URL.
#[test]
fn download_playing_provider_page_is_canonicalized_before_choice_persistence() {
    for page in [
        "https://invidious.example.test/watch?v=dQw4w9WgXcQ",
        "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
    ] {
        let (mut controller, requests, _directory) = source_test_controller();
        let mut video = fixture_download_video();
        video.webpage_url = Some(url::Url::parse(page).unwrap());
        controller.youtube_results = vec![SearchItem::Video(video)];
        controller.refresh_youtube_rows();
        let current = controller.selected_queue_item().unwrap();
        assert_eq!(current.media.webpage_url.as_str(), page);
        controller.playback_queue.begin_now(current, false);
        controller.dispatch(UiAction::Download);
        assert!(
            !controller.manual_downloads.blocked,
            "{}",
            controller.view.status_line
        );
        let saved = controller.store.download_queue().unwrap();
        assert_eq!(saved.entries[0].state, DownloadQueueState::Running);
        let canonical = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        assert_eq!(saved.entries[0].source.download_url.as_str(), canonical);
        assert_eq!(requests.lock().unwrap()[0].source_url.as_str(), canonical);
    }
}

/// A stale pointer index cannot toggle the previously selected media by accident.
#[test]
fn download_mark_at_invalid_index_preserves_existing_marks_and_selection() {
    let (mut controller, _requests, _directory) = source_test_controller();
    controller.dispatch(UiAction::ToggleDownloadMarkAt(usize::MAX));
    assert!(controller.manual_downloads.marks.is_empty());
    controller.dispatch(UiAction::ToggleDownloadMarkAt(0));
    let captured = controller.manual_downloads.marks.clone();
    assert_eq!(captured.len(), 1);
    controller.dispatch(UiAction::ToggleDownloadMarkAt(controller.view.rows.len()));
    assert_eq!(controller.manual_downloads.marks, captured);
    assert_eq!(controller.view.selected, 0);
}

/// Subscription pointer commands select the item pane without trusting the generic row index.
#[test]
fn download_mark_at_subscription_index_claims_the_item_pane() {
    let directory = crate::test_support::canonical_tempdir("subscription download marks");
    let config = Config::for_dir(directory.path().join("config"));
    let (mut controller, _requests) = controller_with_standard_subscription_cache(config);
    controller.view.subscriptions.layout = SubscriptionsLayout::Split;
    controller.view.subscriptions.route = SubscriptionRoute::Sources;
    controller.view.subscriptions.focus = SubscriptionPane::Sources;
    controller.view.subscriptions.description_expanded = false;
    controller.view.subscriptions.selected_item = 0;
    controller.view.rows.clear();
    let expected = controller.view.subscriptions.items[1]
        .media_id
        .clone()
        .unwrap();
    controller.dispatch(UiAction::ToggleDownloadMarkAt(1));
    assert_eq!(controller.view.subscriptions.focus, SubscriptionPane::Items);
    assert_eq!(controller.view.subscriptions.selected_item, 1);
    assert_eq!(
        controller.manual_downloads.marks.len(),
        1,
        "{}",
        controller.view.status_line
    );
    assert_eq!(controller.manual_downloads.marks[0].media_id, expected);
    assert!(controller.view.subscriptions.items[1].download_marked);
    let captured = controller.manual_downloads.marks.clone();
    controller.dispatch(UiAction::ToggleDownloadMarkAt(usize::MAX));
    assert_eq!(controller.manual_downloads.marks, captured);
}

/// Unsupported restored work must reach a terminal state so subsequent jobs can run.
#[cfg(not(feature = "archive-org"))]
#[test]
fn download_queue_without_archive_feature_advances_past_an_unconfirmed_archive_job() {
    let (mut controller, requests, _directory) = source_test_controller();
    controller.dispatch(UiAction::ToggleDownloadMark);
    let youtube = controller.manual_downloads.marks[0].clone();
    controller.manual_downloads.marks.clear();
    let archive_url = url::Url::parse("https://archive.org/download/fixture/chapter.mp3").unwrap();
    let queue = DownloadQueue {
        next_id: 3,
        entries: vec![
            DownloadQueueEntry {
                id: 1,
                source: DownloadSource {
                    media_id: MediaId::new(SourceKind::ArchiveOrg, archive_url.as_str()),
                    kind: MediaKind::Audio,
                    title: "Archive fixture".to_owned(),
                    creator: None,
                    webpage_url: archive_url.clone(),
                    download_url: archive_url,
                    duration_seconds: None,
                },
                format: None,
                state: DownloadQueueState::Queued,
                attempt: 0,
                completed_path: None,
            },
            DownloadQueueEntry {
                id: 2,
                source: youtube,
                format: Some(QueuedDownloadFormat::AudioOnlyWithoutReencoding),
                state: DownloadQueueState::Queued,
                attempt: 0,
                completed_path: None,
            },
        ],
    };
    controller.store.save_download_queue(&queue).unwrap();
    controller.restore_manual_downloads();
    controller.poll_manual_download_queue();
    controller.poll_manual_download_queue();
    let saved = controller.store.download_queue().unwrap();
    assert_eq!(saved.entries[0].state, DownloadQueueState::Failed);
    assert_eq!(saved.entries[1].state, DownloadQueueState::Running);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// A real conflicting queue write must make normal application shutdown report failure.
#[test]
fn download_queue_durability_failure_is_not_overwritten_by_successful_session_shutdown() {
    let (mut controller, requests, _directory) = source_test_controller();
    controller.store = StateStore::open(&controller.config).unwrap();
    controller.dispatch(UiAction::ToggleDownloadMark);
    let document = controller.config.state_dir().join("downloads.toml");
    let original = std::fs::read_to_string(&document).unwrap();
    std::fs::write(
        &document,
        format!("{original}\n# Conflicting external queue edit\n"),
    )
    .unwrap();
    controller.dispatch(UiAction::Download);
    assert!(controller.manual_downloads.blocked);
    assert!(requests.lock().unwrap().is_empty());
    assert!(
        controller.shutdown_for_exit().is_err(),
        "queue durability is required for successful shutdown"
    );
    assert_eq!(controller.shutdown_persistence_succeeded, Some(false));
}

/// Untrusted source metadata is rejected before it can join an existing marked batch.
#[test]
fn download_marks_reject_secret_urls_local_files_and_control_text() {
    for (provider, webpage, playback, title) in [
        (
            SourceKind::GenericYtDlp,
            "https://media.example.test/watch?token=never-persist",
            "https://media.example.test/a.opus",
            "Fixture",
        ),
        (
            SourceKind::Rss,
            "https://media.example.test/episode",
            "https://media.example.test/a.opus?token=never-persist",
            "Fixture",
        ),
        (
            SourceKind::Local,
            "https://media.example.test/local",
            "https://media.example.test/a.opus",
            "Fixture",
        ),
        (
            SourceKind::RemoteFiles,
            "https://media.example.test/a.opus",
            "https://media.example.test/a.opus",
            "Fixture\u{1b}[31m",
        ),
    ] {
        let (mut controller, requests, _directory) = source_test_controller();
        let mut media = resolved_source_fixture(provider, webpage, playback);
        media.title = title.to_owned();
        controller.resolved_direct = Some(media);
        controller.dispatch(UiAction::ToggleDownloadMark);
        assert!(
            controller.manual_downloads.marks.is_empty(),
            "{}",
            controller.view.status_line
        );
        assert!(
            controller
                .store
                .download_queue()
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(requests.lock().unwrap().is_empty());
        assert!(!controller.view.status_line.contains("never-persist"));
    }
}
