//! History preferences cover both session queries and provider search snapshots.

use super::*;

/// Models all query slots, including providers omitted from a minimal build.
fn private_session(screen: StoredScreen) -> SessionState {
    SessionState {
        screen,
        search_text: "private youtube query".to_owned(),
        youtube_music_search_text: "private music query".to_owned(),
        soundcloud_search_text: "private soundcloud query".to_owned(),
        yandex_music_search_text: "private yandex query".to_owned(),
        bandcamp_search_text: "private bandcamp query".to_owned(),
        apple_podcasts_search_text: "private podcast query".to_owned(),
        archive_org_search_text: "private archive query".to_owned(),
        librivox_search_text: "private librivox query".to_owned(),
        radio_filter_text: "private radio filter".to_owned(),
        selected_media: Some(MediaId::new(SourceKind::YouTube, "private-video")),
        selected_row: 8,
        details_scroll: 12,
        ..SessionState::default()
    }
}

/// Seeds one real cached result so a restored query cannot hide behind an empty result list.
fn seed_search(store: &StateStore) {
    store
        .save_youtube_search(
            &SavedYouTubeSearch {
                request: SearchRequest::new("private cached query", SearchTarget::Videos),
                results: vec![SearchItem::Video(subscription_video_summary())],
                next_page: None,
            },
            1,
        )
        .unwrap();
}

/// The privacy policy applies before startup can hydrate any provider or enqueue searches.
#[test]
fn disabled_history_does_not_restore_online_queries_or_cached_results() {
    for screen in [
        StoredScreen::Search,
        StoredScreen::YouTubeMusic,
        StoredScreen::SoundCloud,
        StoredScreen::YandexMusic,
        StoredScreen::Vk,
        StoredScreen::Bandcamp,
        StoredScreen::ApplePodcasts,
        StoredScreen::ArchiveOrg,
        StoredScreen::LibriVox,
        StoredScreen::Web,
    ] {
        let directory = crate::test_support::canonical_tempdir("private startup");
        let mut config = Config::for_dir(directory.path().join("config"));
        config.persistence.save_playback_history = false;
        let store = StateStore::open_in_memory().unwrap();
        store
            .save_session(&private_session(screen.clone()), 1)
            .unwrap();
        seed_search(&store);
        let controller = AppController::new(config, store, None, None);
        assert!(controller.view.search_query.is_empty(), "{screen:?}");
        assert!(controller.youtube_results.is_empty(), "{screen:?}");
        assert!(controller.youtube_music_results.is_empty());
        assert!(controller.youtube_search_query.is_empty());
        assert!(controller.youtube_music_search_query.is_empty());
        assert!(controller.soundcloud.query.is_empty());
        assert!(controller.yandex_music_search_query.is_empty());
        assert!(controller.archive_org_search_query.is_empty());
        assert!(controller.librivox_search_query.is_empty());
        assert!(controller.current_media.is_none());
        assert!(controller.store.youtube_search().unwrap().is_some());
        assert_eq!(
            controller.store.session().unwrap(),
            Some(private_session(screen))
        );
    }
}

/// Disabling persistence does not erase the active query or prevent live results from loading.
#[test]
fn disabled_history_keeps_live_search_in_memory_but_not_across_restart() {
    let directory = crate::test_support::canonical_tempdir("private search restart");
    let mut config = Config::for_dir(directory.path().join("config"));
    config.persistence.save_playback_history = false;
    let store = StateStore::open(&config).unwrap();
    let mut controller = AppController::new(config.clone(), store, None, None);
    controller.view.search_query = "private live query".to_owned();
    controller.handle_provider_response(ProviderResponse::Search {
        generation: controller.search_generation,
        request: SearchRequest::new("private live query", SearchTarget::Videos),
        result: Ok(SearchPage {
            page: 1,
            items: vec![SearchItem::Video(subscription_video_summary())],
            next_page: None,
        }),
    });
    assert_eq!(controller.youtube_results.len(), 1);
    assert_eq!(controller.view.search_query, "private live query");
    assert!(controller.save_session());
    assert!(controller.store.youtube_search().unwrap().is_none());
    let saved = serde_json::to_string(&controller.store.session().unwrap()).unwrap();
    assert!(!saved.contains("private"), "{saved}");
    drop(controller);
    let store = StateStore::open(&config).unwrap();
    let controller = AppController::new(config, store, None, None);
    assert!(controller.view.search_query.is_empty());
    assert!(controller.view.rows.is_empty());
}

/// Applying the preference preserves old data and the live query without saving either again.
#[test]
fn disabling_history_preserves_saved_searches_without_clearing_live_query() {
    let directory = crate::test_support::canonical_tempdir("private preference");
    let config = Config::for_dir(directory.path().join("config"));
    let store = StateStore::open_in_memory().unwrap();
    let mut controller = AppController::new(config, store, None, None);
    seed_search(&controller.store);
    controller
        .store
        .save_session(&private_session(StoredScreen::Search), 1)
        .unwrap();
    controller.view.search_query = "private active query".to_owned();
    controller.dispatch(UiAction::OpenPreferences);
    controller
        .view
        .preferences_popup
        .as_mut()
        .unwrap()
        .save_playback_history = false;
    controller.dispatch(UiAction::SubmitPreferences);
    assert!(!controller.config.persistence.save_playback_history);
    assert!(controller.view.preferences_popup.is_none());
    assert!(controller.store.youtube_search().unwrap().is_some());
    assert_eq!(controller.view.search_query, "private active query");
    assert!(controller.save_session());
    assert_eq!(
        controller.store.session().unwrap(),
        Some(private_session(StoredScreen::Search))
    );
}

/// Late enrichment must not rewrite an old search retained while saving is off.
#[test]
fn disabled_history_does_not_rewrite_saved_search_from_late_details() {
    let directory = crate::test_support::canonical_tempdir("private metadata update");
    let config = Config::for_dir(directory.path().join("config"));
    let store = StateStore::open_in_memory().unwrap();
    let mut controller = AppController::new(config, store, None, None);
    seed_search(&controller.store);
    let saved = controller.store.youtube_search().unwrap().unwrap();
    controller.youtube_search_request = Some(saved.request.clone());
    controller.youtube_results = saved.results.clone();
    controller.refresh_youtube_rows();
    controller.config.persistence.save_playback_history = false;
    let mut details = subscription_video_details("Late private details");
    details.orientation = VideoOrientation::Vertical;
    controller.handle_provider_response(ProviderResponse::Details {
        generation: controller.details_generation,
        result: Ok(details),
    });
    assert_eq!(controller.store.youtube_search().unwrap(), Some(saved));
    assert!(
        controller.view.rows[0].vertical,
        "live metadata still updates"
    );
}

/// The optional transactional backend obeys the same startup and shutdown policy.
#[cfg(feature = "sqlite-state")]
#[test]
fn disabled_history_preserves_sqlite_queries_without_restoring_them() {
    let directory = crate::test_support::canonical_tempdir("private sqlite restart");
    let mut config = Config::for_dir(directory.path().join("config"));
    config.persistence.backend = crate::config::PersistenceBackend::Sqlite;
    let store = StateStore::open(&config).unwrap();
    store
        .save_session(&private_session(StoredScreen::Search), 1)
        .unwrap();
    seed_search(&store);
    config.persistence.save_playback_history = false;
    let mut controller = AppController::new(config.clone(), store, None, None);
    assert!(controller.youtube_results.is_empty());
    assert!(controller.view.search_query.is_empty());
    assert!(controller.store.youtube_search().unwrap().is_some());
    controller.view.search_query = "private query after startup".to_owned();
    assert!(controller.save_session());
    drop(controller);
    let store = StateStore::open(&config).unwrap();
    let controller = AppController::new(config, store, None, None);
    assert!(controller.view.search_query.is_empty());
    assert_eq!(
        controller.store.session().unwrap(),
        Some(private_session(StoredScreen::Search))
    );
}
