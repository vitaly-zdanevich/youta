//! Source-backed now-playing navigation never reloads the active backend.

use super::*;
use std::fs;

/// Installs an accepted queue identity without exercising any provider playback resolver.
fn now_playing_controller(item: QueueItem) -> (AppController, Arc<Mutex<MockPlaybackState>>) {
    let (mut controller, player) = controller_with_mock_statuses([]);
    // Optional enrichment must remain offline even in the all-features suite.
    if let Some(sender) = controller.provider_requests.take() {
        let _ = sender.send(ProviderRequest::Shutdown);
    }
    if let Some(worker) = controller.provider_thread.take() {
        worker.join().unwrap();
    }
    let (sender, requests) = unbounded();
    controller.provider_requests = Some(sender);
    controller.provider_thread = Some(thread::spawn(move || {
        while let Ok(request) = requests.recv() {
            if matches!(request, ProviderRequest::Shutdown) {
                break;
            }
        }
    }));
    controller.current_media = Some(item.media.id.clone());
    controller.playback_queue.begin_now(item, false);
    controller.view.screen = Screen::History;
    controller.view.search_query = "unrelated visible query".to_owned();
    controller.view.rows = vec![RowView {
        title: "Unrelated old row".to_owned(),
        ..RowView::default()
    }];
    (controller, player)
}

/// The selected row, backing queue adapter and Details must all own the same identity.
fn assert_now_playing_selection(
    controller: &mut AppController,
    player: &Arc<Mutex<MockPlaybackState>>,
    screen: Screen,
    selected: usize,
) {
    let queue = controller.playback_queue.clone();
    let id = queue.current().unwrap().media.id.clone();
    controller.dispatch(UiAction::ShowNowPlaying);
    assert_eq!(controller.view.screen, screen);
    assert!(!controller.view.search_editing);
    assert_eq!(controller.view.selected, selected);
    assert_eq!(controller.view.rows[selected].media_id.as_ref(), Some(&id));
    assert_eq!(
        controller.view.details.as_ref().unwrap().media_id.as_ref(),
        Some(&id)
    );
    match screen {
        #[cfg(feature = "bandcamp")]
        Screen::Bandcamp => assert_eq!(controller.bandcamp_results[selected].id, id),
        #[cfg(feature = "apple-podcasts")]
        Screen::ApplePodcasts => assert_eq!(
            queue_item_from_apple_episode(
                &controller.apple_podcast_episodes[selected],
                controller.active_apple_podcast_show.as_ref(),
            )
            .unwrap()
            .media
            .id,
            id,
        ),
        _ => assert_eq!(controller.selected_queue_item().unwrap().media.id, id),
    }
    assert_eq!(controller.playback_queue, queue);
    assert!(player.lock().unwrap().played.is_empty());
    assert!(player.lock().unwrap().commands.is_empty());
}

/// Bandcamp returns to its retained result set instead of showing artwork over History.
#[cfg(feature = "bandcamp")]
#[test]
fn show_now_playing_restores_cached_bandcamp_row_and_details() {
    let summary = bandcamp_summary_fixture(
        "fixture",
        "playing",
        "Playing release",
        BandcampReleaseKind::Track,
    );
    let mut item = fixture_direct_item("Playing release");
    item.media.id = summary.id.clone();
    item.media.webpage_url = summary.webpage_url.clone();
    let (mut controller, player) = now_playing_controller(item);
    controller.bandcamp_results = vec![
        bandcamp_summary_fixture("fixture", "other", "Other", BandcampReleaseKind::Track),
        summary,
    ];
    controller.bandcamp_search_query = "retained Bandcamp query".to_owned();
    assert_now_playing_selection(&mut controller, &player, Screen::Bandcamp, 1);
    assert_eq!(controller.view.search_query, "retained Bandcamp query");
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .description
            .contains("Fixture Artist")
    );
}

/// Yandex uses its actual typed track rows and preserves its source-specific metadata.
#[cfg(feature = "yandex-music")]
#[test]
fn show_now_playing_restores_cached_yandex_row_and_details() {
    let track = yandex_music_track_fixture();
    let item = queue_item_from_yandex_music_track(&track);
    let mut other = track.clone();
    other.id = "other".to_owned();
    let (mut controller, player) = now_playing_controller(item);
    controller.yandex_music_rows = vec![
        YandexMusicRow::Track(Box::new(other)),
        YandexMusicRow::Track(Box::new(track)),
    ];
    controller.yandex_music_search_query = "retained Yandex query".to_owned();
    assert_now_playing_selection(&mut controller, &player, Screen::YandexMusic, 1);
    assert_eq!(controller.view.search_query, "retained Yandex query");
}

/// Episode navigation returns to the open podcast, not its show catalogue.
#[cfg(feature = "apple-podcasts")]
#[test]
fn show_now_playing_restores_cached_apple_episode_and_details() {
    let show = apple_podcast_show_fixture(1001, "Fixture show");
    let playing = apple_podcast_episode_fixture(
        1001,
        1003,
        "Playing episode",
        Some("https://media.example/playing.mp3"),
    );
    let item = queue_item_from_apple_episode(&playing, Some(&show)).unwrap();
    let (mut controller, player) = now_playing_controller(item);
    controller.active_apple_podcast_show = Some(show);
    controller.apple_podcast_episodes = vec![
        apple_podcast_episode_fixture(1001, 1002, "Other", Some("https://media.example/other.mp3")),
        playing,
    ];
    controller.apple_podcasts_route = ApplePodcastsRoute::Shows;
    assert_now_playing_selection(&mut controller, &player, Screen::ApplePodcasts, 1);
    assert_eq!(
        controller.apple_podcasts_route,
        ApplePodcastsRoute::Episodes
    );
    assert_eq!(
        controller.view.details.as_ref().unwrap().description,
        "Mock episode description"
    );
}

/// The playing LibriVox section restores its book route with complete author links.
#[cfg(feature = "librivox")]
#[test]
fn show_now_playing_restores_cached_librivox_section_and_details() {
    let book = librivox_book_fixture();
    let item = queue_item_from_librivox_section(&book, &book.sections[1]);
    let (mut controller, player) = now_playing_controller(item);
    controller.active_librivox_book = Some(book);
    controller.librivox_route = LibrivoxRoute::Books;
    assert_now_playing_selection(&mut controller, &player, Screen::LibriVox, 1);
    assert_eq!(controller.librivox_route, LibrivoxRoute::Book);
    assert!(!controller.view.details.as_ref().unwrap().links.is_empty());
}

/// Web's parent row must not shift the playing entry onto a different file.
#[cfg(feature = "web-browser")]
#[test]
fn show_now_playing_restores_cached_web_entry_with_parent_offset() {
    use crate::web_browser::{WebDirectoryListing, WebEntry, WebEntryKind};
    let playing = WebEntry {
        url: url::Url::parse("https://example.org/music/playing.mp3").unwrap(),
        name: "Playing audio".to_owned(),
        kind: WebEntryKind::Audio,
    };
    let item = crate::app::web::queue_item_from_web(&playing).unwrap();
    let (mut controller, player) = now_playing_controller(item);
    let directory = url::Url::parse("https://example.org/music/").unwrap();
    controller.web.query = directory.to_string();
    controller.web.listing = Some(WebDirectoryListing {
        url: directory,
        parent: Some(url::Url::parse("https://example.org/").unwrap()),
        entries: vec![playing],
        truncated: false,
    });
    assert_now_playing_selection(&mut controller, &player, Screen::Web, 1);
    assert_eq!(controller.view.search_query, "https://example.org/music/");
}

/// Direct-only providers reveal their retained authoritative adapter row, not a fake tab.
#[test]
fn show_now_playing_restores_cached_direct_provider_rows() {
    for source in [
        SourceKind::WikimediaCommons,
        SourceKind::Odysee,
        SourceKind::Rumble,
        SourceKind::Bilibili,
        SourceKind::PeerTube,
        SourceKind::Funkwhale,
        SourceKind::Vimeo,
        SourceKind::RuTube,
        SourceKind::Jamendo,
        SourceKind::SoundStream,
        SourceKind::LitRes,
        SourceKind::GenericYtDlp,
        SourceKind::Vk,
        SourceKind::Telegram,
        SourceKind::Other("fixture-plugin".to_owned()),
    ] {
        let media = ResolvedDirectMedia {
            source: source.clone(),
            external_id: "playing".to_owned(),
            title: format!("{source} playing"),
            row_subtitle: "Provider summary".to_owned(),
            description: "Full provider description".to_owned(),
            license: "Fixture license".to_owned(),
            published: None,
            artwork_url: None,
            duration_seconds: Some(120),
            playback_url: Some(url::Url::parse("https://media.example/playing.mp3").unwrap()),
            webpage_url: Some(url::Url::parse("https://media.example/playing").unwrap()),
            status_line: "Fixture resolved".to_owned(),
        };
        let item = queue_item_from_resolved(&media).unwrap();
        let (mut controller, player) = now_playing_controller(item);
        controller.resolved_direct = Some(media);
        assert_now_playing_selection(&mut controller, &player, Screen::Search, 0);
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            "Full provider description"
        );
    }
}

/// Accepted direct queue metadata is sufficient for a real reversible adapter, not an orphan Details panel.
#[test]
fn show_now_playing_retains_direct_queue_metadata_without_a_provider_result() {
    for source in [SourceKind::RemoteFiles, SourceKind::Rss, SourceKind::Odysee] {
        let mut item = fixture_direct_item("Already accepted direct media");
        item.media.id.source = source;
        item.media.description = Some("Only known accepted metadata".into());
        let (mut controller, player) = now_playing_controller(item.clone());
        controller.remember_now_playing_context(&item);
        assert_now_playing_selection(&mut controller, &player, Screen::Search, 0);
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            "Only known accepted metadata"
        );
    }
}

/// A replaced Bandcamp search remains reachable with Back after revealing playback.
#[cfg(feature = "bandcamp")]
#[test]
fn show_now_playing_retains_bandcamp_context_and_restores_newer_search() {
    let playing =
        bandcamp_summary_fixture("fixture", "playing", "Playing", BandcampReleaseKind::Track);
    let newer = bandcamp_summary_fixture(
        "fixture",
        "newer",
        "Newer search",
        BandcampReleaseKind::Track,
    );
    let mut item = fixture_direct_item("Playing");
    item.media.id = playing.id.clone();
    item.media.webpage_url = playing.webpage_url.clone();
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.bandcamp_results = vec![playing];
    controller.remember_now_playing_context(&item);
    controller.bandcamp_results = vec![newer.clone()];
    controller.bandcamp_search_query = "newer search".into();
    controller.bandcamp_page = 3;
    controller.bandcamp_next_page = Some(4);
    assert_now_playing_selection(&mut controller, &player, Screen::Bandcamp, 0);
    controller.dispatch(UiAction::GoBack);
    assert_eq!(controller.bandcamp_results, vec![newer]);
    assert_eq!(controller.view.search_query, "newer search");
    assert_eq!(controller.bandcamp_page, 3);
    assert_eq!(controller.bandcamp_next_page, Some(4));
    assert!(player.lock().unwrap().played.is_empty());
}

/// Opening a second podcast must not discard the accepted episode's own show.
#[cfg(feature = "apple-podcasts")]
#[test]
fn show_now_playing_retains_apple_context_and_restores_newer_show() {
    let show = apple_podcast_show_fixture(1001, "Playing show");
    let playing = apple_podcast_episode_fixture(
        1001,
        1003,
        "Playing episode",
        Some("https://media.example/playing.mp3"),
    );
    let item = queue_item_from_apple_episode(&playing, Some(&show)).unwrap();
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.active_apple_podcast_show = Some(show);
    controller.apple_podcast_episodes = vec![playing];
    controller.remember_now_playing_context(&item);
    controller.active_apple_podcast_show = Some(apple_podcast_show_fixture(2001, "Newer show"));
    controller.apple_podcast_episodes = vec![apple_podcast_episode_fixture(
        2001,
        2002,
        "Other episode",
        Some("https://media.example/other.mp3"),
    )];
    controller.apple_podcasts_route = ApplePodcastsRoute::Episodes;
    controller.apple_podcasts_search_query = "newer podcast".into();
    assert_now_playing_selection(&mut controller, &player, Screen::ApplePodcasts, 0);
    assert_eq!(
        controller.active_apple_podcast_show.as_ref().unwrap().title,
        "Playing show"
    );
    controller.dispatch(UiAction::GoBack);
    assert_eq!(
        controller.active_apple_podcast_show.as_ref().unwrap().title,
        "Newer show"
    );
    assert_eq!(controller.apple_podcast_episodes[0].episode_id, 2002);
    assert_eq!(controller.view.search_query, "newer podcast");
}

/// Accepted direct metadata survives a later search without replacing it irreversibly.
#[test]
fn show_now_playing_retains_direct_context_after_accepted_playback_only() {
    let media = ResolvedDirectMedia {
        source: SourceKind::Vimeo,
        external_id: "playing".into(),
        title: "Playing direct item".into(),
        row_subtitle: "Resolved artist".into(),
        description: "Authoritative resolved description".into(),
        license: "Fixture license".into(),
        published: None,
        artwork_url: None,
        duration_seconds: Some(120),
        playback_url: Some(url::Url::parse("https://media.example/playing.mp3").unwrap()),
        webpage_url: Some(url::Url::parse("https://vimeo.com/123").unwrap()),
        status_line: "Resolved".into(),
    };
    let item = queue_item_from_resolved(&media).unwrap();
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.resolved_direct = Some(media);
    controller.play_queue_item_with_origin_and_input(item, true, None, None);
    player.lock().unwrap().played.clear();
    player.lock().unwrap().commands.clear();
    let newer = VideoSummary {
        video_id: "abcdefghijk".into(),
        title: "Newer search result".into(),
        ..subscription_video_summary()
    };
    controller.resolved_direct = None;
    controller.youtube_results = vec![SearchItem::Video(newer)];
    controller.youtube_search_query = "newer YouTube search".into();
    controller.view.screen = Screen::Search;
    controller.view.search_query = "newer YouTube search".into();
    controller.view.search_editing = true;
    assert_now_playing_selection(&mut controller, &player, Screen::Search, 0);
    assert_eq!(
        controller.view.details.as_ref().unwrap().description,
        "Authoritative resolved description"
    );
    controller.dispatch(UiAction::GoBack);
    assert_eq!(controller.view.rows[0].title, "Newer search result");
    assert_eq!(controller.view.search_query, "newer YouTube search");
    assert!(controller.resolved_direct.is_none());
}

/// A fresh History/queue replay has a canonical YouTube adapter even without cached search data.
#[test]
fn show_now_playing_restores_uncached_accepted_youtube_without_losing_newer_search() {
    let item = fixture_youtube_item("Accepted historical video");
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.youtube_results.clear();
    controller.play_queue_item_with_origin_and_input(item.clone(), true, None, None);
    player.lock().unwrap().played.clear();
    player.lock().unwrap().commands.clear();
    let mut newer = subscription_video_summary();
    newer.video_id = "abcdefghijk".into();
    newer.title = "Newer source result".into();
    controller.youtube_results = vec![SearchItem::Video(newer)];
    controller.youtube_search_query = "newer untouched query".into();
    controller.next_youtube_page = Some(4);
    assert_now_playing_selection(&mut controller, &player, Screen::Search, 0);
    assert_eq!(controller.view.rows[0].title, "Accepted historical video");
    assert!(controller.next_youtube_page.is_none());
    controller.dispatch(UiAction::GoBack);
    assert_eq!(controller.view.search_query, "newer untouched query");
    assert_eq!(controller.view.rows[0].title, "Newer source result");
    assert_eq!(controller.next_youtube_page, Some(4));
}

/// An RSS episode reveals its real subscribed feed and correctly aligned episode row.
#[cfg(feature = "rss")]
#[test]
fn show_now_playing_restores_cached_rss_episode() {
    let feed_url = url::Url::parse("https://podcasts.example/feed.xml").unwrap();
    let feed = fixture_rss_feed(&feed_url, &["one", "two"]);
    let items: Vec<_> = feed
        .episodes
        .iter()
        .map(|episode| {
            SearchItem::PodcastEpisode(Box::new(podcast_episode_summary(&feed, episode, &feed_url)))
        })
        .collect();
    let SearchItem::PodcastEpisode(episode) = &items[1] else {
        unreachable!()
    };
    let item = queue_item_from_podcast_episode(episode).unwrap();
    let (mut controller, player) = now_playing_controller(item.clone());
    controller
        .subscription_tree
        .subscribe_rss_feed("Fixture feed", feed_url.as_str())
        .unwrap();
    controller.cache_subscription_video_page(
        feed_url.as_str(),
        SearchPage {
            page: 1,
            items,
            next_page: None,
        },
    );
    controller.view.screen = Screen::Search;
    controller.view.search_query = "unsent outgoing search draft".into();
    controller.view.search_editing = true;
    let queue = controller.playback_queue.clone();
    controller.dispatch(UiAction::ShowNowPlaying);
    assert_eq!(controller.view.screen, Screen::Subscriptions);
    assert!(!controller.view.search_editing);
    assert_eq!(
        controller.youtube_search_query,
        "unsent outgoing search draft"
    );
    assert_eq!(controller.view.subscriptions.selected_item, 1);
    assert_eq!(
        controller.view.subscriptions.items[1].media_id.as_ref(),
        Some(&item.media.id)
    );
    assert_eq!(
        controller.selected_queue_item().unwrap().media.id,
        item.media.id
    );
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .description
            .ends_with("Episode 2 description.")
    );
    assert_eq!(controller.playback_queue, queue);
    assert!(player.lock().unwrap().played.is_empty());
}

/// The source marker follows accepted provenance, not whichever tab is being browsed.
#[test]
fn show_now_playing_marker_preserves_music_provenance_with_duplicate_cached_ids() {
    for music in [false, true] {
        let video = subscription_video_summary();
        let mut item = fixture_direct_item("Playing YouTube item");
        item.media.id = MediaId::new(SourceKind::YouTube, &video.video_id);
        item.media.webpage_url =
            url::Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        let (mut controller, player) = now_playing_controller(item.clone());
        controller.youtube_results = vec![SearchItem::Video(video.clone())];
        controller.youtube_music_results = vec![SearchItem::Video(video)];
        let origin = if music {
            AutoplayOrigin::YouTubeMusic {
                generation: 0,
                index: 0,
            }
        } else {
            AutoplayOrigin::YouTube {
                generation: 0,
                index: 0,
            }
        };
        controller.remember_now_playing_context(&item);
        controller.update_now_playing_screen(&item, Some(&origin));
        let expected = if music {
            Screen::YouTubeMusic
        } else {
            Screen::Search
        };
        assert_eq!(controller.view.playing_screen, Some(expected));
        if !music {
            controller.view.screen = Screen::YouTubeMusic;
        }
        assert_now_playing_selection(&mut controller, &player, expected, 0);
        controller.view.playback.paused = true;
        assert_eq!(controller.view.playing_screen, Some(expected));
        controller.reset_playback_state();
        assert_eq!(controller.view.playing_screen, None);
    }
}

/// Direct BBC pages use the real direct adapter rather than a non-existent station row.
#[test]
fn show_now_playing_marker_uses_direct_bbc_adapter_destination() {
    let media = ResolvedDirectMedia {
        source: SourceKind::BbcRadio,
        external_id: "programme".into(),
        title: "BBC programme".into(),
        row_subtitle: String::new(),
        description: "Resolved public programme metadata".into(),
        license: String::new(),
        published: None,
        artwork_url: None,
        duration_seconds: Some(100),
        playback_url: Some(url::Url::parse("https://media.example/programme.mp3").unwrap()),
        webpage_url: Some(url::Url::parse("https://www.bbc.co.uk/sounds/play/programme").unwrap()),
        status_line: String::new(),
    };
    let item = queue_item_from_resolved(&media).unwrap();
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.resolved_direct = Some(media);
    controller.remember_now_playing_context(&item);
    controller.update_now_playing_screen(&item, None);
    assert_eq!(controller.view.playing_screen, Some(Screen::Search));
    assert_now_playing_selection(&mut controller, &player, Screen::Search, 0);
}

/// A focused page cannot roll a newer search back through its one-slot history.
#[cfg(feature = "bandcamp")]
#[test]
fn show_now_playing_back_does_not_undo_a_newer_search() {
    let playing =
        bandcamp_summary_fixture("fixture", "playing", "Playing", BandcampReleaseKind::Track);
    let mut item = fixture_direct_item("Playing");
    item.media.id = playing.id.clone();
    item.media.webpage_url = playing.webpage_url.clone();
    let (mut controller, _) = now_playing_controller(item.clone());
    controller.bandcamp_results = vec![playing];
    controller.remember_now_playing_context(&item);
    controller.bandcamp_results.clear();
    controller.dispatch(UiAction::ShowNowPlaying);
    controller.supersede_search_generation();
    assert!(!controller.restore_now_playing_location());
    assert_eq!(controller.bandcamp_results[0].id, item.media.id);
}

/// Returning from another tab must rebuild Local rows, not reuse History's old rows.
#[test]
fn show_now_playing_restores_cached_local_rows_without_replaying() {
    let temporary = crate::test_support::canonical_tempdir("now-playing-local");
    let path = temporary.path().join("playing.mp3");
    fs::write(&path, b"fixture").unwrap();
    let item = queue_item_from_local(&local_media_item_without_probe(path.clone())).unwrap();
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.local_listing = Some(crate::local_browser::LocalDirectoryListing {
        path: temporary.path().to_path_buf(),
        parent: None,
        entries: vec![crate::local_browser::LocalEntry {
            name: "playing.mp3".into(),
            path,
            kind: crate::local_browser::LocalEntryKind::Audio,
            size_bytes: Some(7),
            image_dimensions: None,
            directory_identity: None,
        }],
        truncated: false,
        inspected_entries: 1,
    });
    let queue = controller.playback_queue.clone();
    controller.dispatch(UiAction::ShowNowPlaying);
    assert_eq!(controller.view.screen, Screen::Local);
    assert_eq!(
        controller.view.rows[0].media_id.as_ref(),
        Some(&item.media.id)
    );
    assert_eq!(
        controller.view.details.as_ref().unwrap().media_id.as_ref(),
        Some(&item.media.id)
    );
    assert_eq!(controller.playback_queue, queue);
    assert!(player.lock().unwrap().played.is_empty());
}

/// A later directory response from this explicit reveal cannot overwrite another tab.
#[test]
fn show_now_playing_local_directory_recovery_rejects_late_route_completion() {
    let temporary = crate::test_support::canonical_tempdir("now-playing-local-late");
    let path = temporary.path().join("playing.mp3");
    fs::write(&path, b"fixture").unwrap();
    let item = queue_item_from_local(&local_media_item_without_probe(path.clone())).unwrap();
    let (mut controller, player) = now_playing_controller(item);
    let (sender, _requests) = unbounded();
    controller.local_browse_requests = Some(sender);
    controller.dispatch(UiAction::ShowNowPlaying);
    assert_eq!(controller.view.screen, Screen::Local);
    assert_eq!(
        controller
            .pending_local_reselection
            .as_ref()
            .map(|(_, path)| path),
        Some(&path)
    );
    let generation = controller.local_generation;
    controller.show_screen(Screen::Search);
    let rows = controller.view.rows.clone();
    controller.handle_local_directory_response(
        generation,
        Ok(crate::local_browser::LocalDirectoryListing {
            path: temporary.path().to_owned(),
            parent: None,
            entries: Vec::new(),
            truncated: false,
            inspected_entries: 0,
        }),
    );
    assert_eq!(controller.view.screen, Screen::Search);
    assert_eq!(controller.view.rows, rows);
    assert!(controller.local_listing.is_none());
    assert!(player.lock().unwrap().played.is_empty());
}

/// Retained authenticated metadata reopens the actual track without resolving audio again.
#[cfg(feature = "yandex-music")]
#[test]
fn show_now_playing_retains_yandex_context_and_restores_newer_rows() {
    let track = yandex_music_track_fixture();
    let item = queue_item_from_yandex_music_track(&track);
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.yandex_music_rows = vec![YandexMusicRow::Track(Box::new(track.clone()))];
    controller.remember_now_playing_context(&item);
    let mut newer = track;
    newer.id = "newer".into();
    controller.yandex_music_rows = vec![YandexMusicRow::Track(Box::new(newer))];
    controller.yandex_music_search_query = "newer Yandex query".into();
    assert_now_playing_selection(&mut controller, &player, Screen::YandexMusic, 0);
    controller.dispatch(UiAction::GoBack);
    assert!(
        matches!(&controller.yandex_music_rows[0], YandexMusicRow::Track(track) if track.id == "newer")
    );
    assert_eq!(controller.view.search_query, "newer Yandex query");
}

/// A canonical accepted Yandex replay reuses the same typed adapter as playback itself.
#[cfg(feature = "yandex-music")]
#[test]
fn show_now_playing_retains_uncached_yandex_replay_and_rejects_mismatched_identity() {
    let item = queue_item_from_yandex_music_track(&yandex_music_track_fixture());
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.remember_now_playing_context(&item);
    assert_now_playing_selection(&mut controller, &player, Screen::YandexMusic, 0);

    let mut mismatched = item;
    mismatched.media.id.external_id = "different-track".into();
    let (mut controller, _) = now_playing_controller(mismatched.clone());
    controller.remember_now_playing_context(&mismatched);
    controller.dispatch(UiAction::ShowNowPlaying);
    assert_eq!(controller.view.screen, Screen::History);
}

/// A book's complete real section list remains available after another book was opened.
#[cfg(feature = "librivox")]
#[test]
fn show_now_playing_retains_librivox_context_and_restores_newer_book() {
    let book = librivox_book_fixture();
    let item = queue_item_from_librivox_section(&book, &book.sections[1]);
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.active_librivox_book = Some(book.clone());
    controller.remember_now_playing_context(&item);
    let mut newer = book;
    newer.book_id += 1;
    let newer_id = newer.book_id;
    controller.active_librivox_book = Some(newer);
    controller.librivox_route = LibrivoxRoute::Book;
    controller.librivox_search_query = "newer audiobook".into();
    assert_now_playing_selection(&mut controller, &player, Screen::LibriVox, 1);
    controller.dispatch(UiAction::GoBack);
    assert_eq!(
        controller.active_librivox_book.as_ref().unwrap().book_id,
        newer_id
    );
    assert_eq!(controller.view.search_query, "newer audiobook");
}

/// A fresh History replay loads its exact book and selects the real section without starting audio.
#[cfg(feature = "librivox")]
#[test]
fn show_now_playing_recovers_uncached_librivox_by_exact_book_and_section() {
    let book = librivox_book_fixture();
    let item = queue_item_from_librivox_section(&book, &book.sections[1]);
    let (mut controller, player) = now_playing_controller(item.clone());
    let mut prior_book = book.clone();
    prior_book.book_id += 1;
    controller.librivox_books = vec![prior_book.clone()];
    controller.librivox_search_query = "preserved catalogue".into();
    let queue = controller.playback_queue.clone();
    controller.dispatch(UiAction::ShowNowPlaying);
    assert_eq!(controller.view.screen, Screen::LibriVox);
    assert!(
        matches!(controller.pending_librivox_request, Some(PendingLibrivoxRequest::Book { book_id, .. }) if book_id == book.book_id)
    );
    controller.handle_provider_response(ProviderResponse::LibrivoxBook {
        generation: controller.librivox_generation,
        book_id: book.book_id,
        result: Ok(Box::new(book)),
    });
    assert_eq!(controller.view.selected, 1);
    assert_eq!(
        controller.view.rows[1].media_id.as_ref(),
        Some(&item.media.id)
    );
    assert_eq!(
        controller.view.details.as_ref().unwrap().media_id.as_ref(),
        Some(&item.media.id)
    );
    assert_eq!(controller.playback_queue, queue);
    assert!(player.lock().unwrap().played.is_empty());
    controller.dispatch(UiAction::GoBack);
    assert_eq!(controller.librivox_books[0].book_id, prior_book.book_id);
    assert_eq!(controller.view.search_query, "preserved catalogue");
}

/// Exact metadata completions cannot navigate after the user leaves their loading route.
#[cfg(feature = "librivox")]
#[test]
fn show_now_playing_librivox_lookup_rejects_late_completions() {
    for change_media in [false, true] {
        let book = librivox_book_fixture();
        let item = queue_item_from_librivox_section(&book, &book.sections[1]);
        let (mut controller, player) = now_playing_controller(item);
        controller.dispatch(UiAction::ShowNowPlaying);
        assert_eq!(controller.view.screen, Screen::LibriVox);
        let generation = controller.librivox_generation;
        if change_media {
            controller.current_media = Some(MediaId::new(SourceKind::YouTube, "abcdefghijk"));
        } else {
            controller.show_screen(Screen::Search);
        }
        let screen = controller.view.screen;
        controller.handle_provider_response(ProviderResponse::LibrivoxBook {
            generation,
            book_id: book.book_id,
            result: Ok(Box::new(book)),
        });
        assert_eq!(controller.view.screen, screen);
        assert!(controller.active_librivox_book.is_none());
        assert!(player.lock().unwrap().played.is_empty());
    }
}

/// Starting a newer search draft revokes navigation even if the editor closes before the response.
#[cfg(feature = "librivox")]
#[test]
fn show_now_playing_librivox_lookup_preserves_newer_search_editor() {
    for editor_state in ["editing", "closed", "projected"] {
        let book = librivox_book_fixture();
        let item = queue_item_from_librivox_section(&book, &book.sections[1]);
        let (mut controller, player) = now_playing_controller(item);
        controller.dispatch(UiAction::ShowNowPlaying);
        let generation = controller.librivox_generation;
        if editor_state == "projected" {
            controller.view.search_editing = true;
        } else {
            controller.dispatch(UiAction::BeginSearch);
        }
        controller.view.search_query = "newer audiobook draft".into();
        if editor_state == "closed" {
            controller.dispatch(UiAction::CancelSearch);
        }
        controller.view.details_focused = false;
        let rows = controller.view.rows.clone();
        let queue = controller.playback_queue.clone();
        controller.handle_provider_response(ProviderResponse::LibrivoxBook {
            generation,
            book_id: book.book_id,
            result: Ok(Box::new(book)),
        });
        assert_eq!(controller.view.search_query, "newer audiobook draft");
        assert_eq!(controller.view.search_editing, editor_state != "closed");
        assert!(!controller.view.details_focused, "{editor_state}");
        assert_eq!(controller.view.rows, rows, "{editor_state}");
        assert!(controller.active_librivox_book.is_none(), "{editor_state}");
        assert!(controller.pending_librivox_request.is_none());
        assert_eq!(controller.playback_queue, queue);
        assert!(player.lock().unwrap().played.is_empty());
    }
}

/// A retained Web entry has a real listing and a reversible newer directory, without HTTP.
#[cfg(feature = "web-browser")]
#[test]
fn show_now_playing_retains_web_context_and_restores_newer_directory() {
    use crate::web_browser::{WebDirectoryListing, WebEntry, WebEntryKind};
    let entry = WebEntry {
        url: url::Url::parse("https://example.org/old/playing.mp3").unwrap(),
        name: "Playing".into(),
        kind: WebEntryKind::Audio,
    };
    let item = web::queue_item_from_web(&entry).unwrap();
    let (mut controller, player) = now_playing_controller(item.clone());
    controller.web.listing = Some(WebDirectoryListing {
        url: url::Url::parse("https://example.org/old/").unwrap(),
        parent: None,
        entries: vec![entry],
        truncated: false,
    });
    controller.web.query = "https://example.org/old/".into();
    controller.remember_now_playing_context(&item);
    controller.web.listing = Some(WebDirectoryListing {
        url: url::Url::parse("https://example.org/new/").unwrap(),
        parent: None,
        entries: Vec::new(),
        truncated: false,
    });
    controller.web.query = "https://example.org/new/".into();
    assert_now_playing_selection(&mut controller, &player, Screen::Web, 0);
    controller.dispatch(UiAction::GoBack);
    assert_eq!(
        controller.web.listing.as_ref().unwrap().url.as_str(),
        "https://example.org/new/"
    );
    assert_eq!(controller.view.search_query, "https://example.org/new/");
    assert!(controller.web.worker.is_none());
}
