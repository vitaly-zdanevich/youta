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
        SourceKind::Funkwhale,
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
