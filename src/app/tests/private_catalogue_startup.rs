//! Private sessions populate online catalogues only after an explicit request.

use super::*;

/// Starts on YouTube so each test can replace workers before changing tabs.
fn controller() -> (tempfile::TempDir, AppController) {
    let directory = crate::test_support::canonical_tempdir("private catalogue");
    let mut config = Config::for_dir(directory.path().join("config"));
    config.persistence.save_playback_history = false;
    let store = StateStore::open_in_memory().unwrap();
    (directory, AppController::new(config, store, None, None))
}

/// Allows only the selected fixture author's optional metadata lookup, never a catalogue load.
#[cfg(feature = "librivox")]
fn assert_only_selected_librivox_details(requests: &Receiver<ProviderRequest>) {
    for request in requests.try_iter() {
        match request {
            #[cfg(feature = "wikidata")]
            ProviderRequest::Wikidata {
                kind: crate::providers::wikidata::WikidataExternalKind::LibriVoxAuthor,
                external_id,
                ..
            } => assert_eq!(external_id, librivox_author_fixture().author_id.to_string()),
            ProviderRequest::LibrivoxSearch { .. } => {
                panic!("unexpected automatic LibriVox catalogue search")
            }
            _ => panic!("unexpected request other than selected LibriVox author details"),
        }
    }
}

/// Intercepts the authenticated lane without sending real requests.
#[cfg(feature = "yandex-music")]
fn capture_yandex_requests(controller: &mut AppController) -> Receiver<YandexMusicWorkerRequest> {
    controller
        .yandex_music_stopping
        .store(true, AtomicOrdering::Release);
    controller.yandex_music_requests.take();
    if let Some(worker) = controller.yandex_music_thread.take() {
        worker.join().unwrap();
    }
    let (requests, captured) = unbounded();
    controller.yandex_music_requests = Some(requests);
    controller
        .yandex_music_stopping
        .store(false, AtomicOrdering::Release);
    controller.config.providers.yandex_music_token = Some("fixture-token".to_owned());
    captured
}

/// Merely revisiting an empty LibriVox tab never starts or retries its catalogue.
#[cfg(feature = "librivox")]
#[test]
fn private_librivox_navigation_waits_for_explicit_search() {
    let (_directory, mut controller) = controller();
    let requests = capture_controller_provider_requests(&mut controller);
    for _ in 0..3 {
        controller.show_screen(Screen::LibriVox);
        controller.populate_librivox();
        assert!(controller.view.rows.is_empty());
        assert!(controller.pending_librivox_request.is_none());
        assert!(controller.view.search_activity.is_none());
        assert_eq!(controller.librivox_generation, 0);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        controller.show_screen(Screen::Search);
    }
    controller.config.persistence.save_playback_history = true;
    controller.show_screen(Screen::LibriVox);
    assert!(matches!(
        requests.try_recv().unwrap(),
        ProviderRequest::LibrivoxSearch { .. }
    ));
}

/// A restored LibriVox tab starts without scheduling its public default catalogue.
#[cfg(feature = "librivox")]
#[test]
fn private_librivox_startup_waits_for_explicit_search() {
    let directory = crate::test_support::canonical_tempdir("private LibriVox startup");
    let mut config = Config::for_dir(directory.path().join("config"));
    config.persistence.save_playback_history = false;
    let store = StateStore::open_in_memory().unwrap();
    store
        .save_session(
            &SessionState {
                screen: StoredScreen::LibriVox,
                ..SessionState::default()
            },
            1,
        )
        .unwrap();
    let mut controller = AppController::new(config, store, None, None);
    let requests = capture_controller_provider_requests(&mut controller);
    controller.tick();
    assert!(controller.view.rows.is_empty());
    assert!(controller.pending_librivox_request.is_none());
    assert_eq!(controller.librivox_generation, 0);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
}

/// An explicit empty browse or text search remains usable without persistence.
#[cfg(feature = "librivox")]
#[test]
fn private_librivox_explicit_search_keeps_live_results() {
    let (_directory, mut controller) = controller();
    let requests = capture_controller_provider_requests(&mut controller);
    controller.view.screen = Screen::LibriVox;
    for query in ["", "memoir"] {
        controller.view.search_query = query.to_owned();
        controller.dispatch(UiAction::SubmitSearch);
        let generation = match requests.try_recv().unwrap() {
            ProviderRequest::LibrivoxSearch {
                generation,
                query: accepted,
                request,
            } => {
                assert_eq!(accepted, query);
                assert_eq!(
                    request.title.as_deref(),
                    (!query.is_empty()).then_some(query)
                );
                generation
            }
            _ => panic!("expected the explicit LibriVox request"),
        };
        controller.handle_provider_response(ProviderResponse::LibrivoxSearch {
            generation,
            query: query.to_owned(),
            result: Ok(LibrivoxSearchPage {
                books: vec![librivox_book_fixture()],
                offset: 0,
                next_offset: None,
            }),
        });
        controller.show_screen(Screen::Search);
        controller.show_screen(Screen::LibriVox);
        assert_eq!(controller.view.rows.len(), 1);
        assert_eq!(controller.librivox_books, vec![librivox_book_fixture()]);
        assert_only_selected_librivox_details(&requests);
    }
}

/// Disabling history changes future implicit loads, not the active book list.
#[cfg(feature = "librivox")]
#[test]
fn disabling_history_retains_live_librivox_results() {
    let (_directory, mut controller) = controller();
    let requests = capture_controller_provider_requests(&mut controller);
    controller.config.persistence.save_playback_history = true;
    controller.librivox_books = vec![librivox_book_fixture()];
    controller.show_screen(Screen::LibriVox);
    let rows = controller.view.rows.clone();
    controller.config.persistence.save_playback_history = false;
    controller.show_screen(Screen::Search);
    controller.show_screen(Screen::LibriVox);
    assert_eq!(controller.view.rows, rows);
    assert_only_selected_librivox_details(&requests);
}

/// The catalogue exception does not turn other providers' blank input into searches.
#[test]
fn empty_queries_still_require_text_for_non_catalogue_providers() {
    let (_directory, mut controller) = controller();
    for screen in [
        Screen::Search,
        Screen::YouTubeMusic,
        Screen::SoundCloud,
        Screen::Bandcamp,
        Screen::ApplePodcasts,
        Screen::TrackerMusic,
    ] {
        controller.view.screen = screen;
        controller.view.search_query.clear();
        controller.dispatch(UiAction::SubmitSearch);
        assert_eq!(
            controller.view.status_line, "Enter a search query",
            "{screen:?}"
        );
    }
}

/// Private Yandex startup neither loads recommendations nor opens unsolicited setup.
#[cfg(feature = "yandex-music")]
#[test]
fn private_yandex_startup_waits_for_explicit_browse() {
    for token in [None, Some(String::new())] {
        let directory = crate::test_support::canonical_tempdir("private Yandex startup");
        let mut config = Config::for_dir(directory.path().join("config"));
        config.persistence.save_playback_history = false;
        // Empty tokens fail client validation before any network request.
        config.providers.yandex_music_token = token;
        let store = StateStore::open_in_memory().unwrap();
        store
            .save_session(
                &SessionState {
                    screen: StoredScreen::YandexMusic,
                    ..SessionState::default()
                },
                1,
            )
            .unwrap();
        let controller = AppController::new(config, store, None, None);
        assert!(controller.view.rows.is_empty());
        assert!(controller.view.search_activity.is_none());
        assert!(controller.view.yandex_music_setup_popup.is_none());
        assert_eq!(controller.yandex_music_generation, 0);
        assert!(!controller.view.status_line.contains("Loading"));
    }
}

/// Tab changes never bootstrap My Wave, but explicit Home and searches still do.
#[cfg(feature = "yandex-music")]
#[test]
fn private_yandex_navigation_waits_for_explicit_browse() {
    let (_directory, mut controller) = controller();
    let requests = capture_yandex_requests(&mut controller);
    for _ in 0..3 {
        controller.show_screen(Screen::YandexMusic);
        controller.populate_local_screen();
        assert!(controller.view.rows.is_empty());
        assert!(controller.view.search_activity.is_none());
        assert_eq!(controller.yandex_music_generation, 0);
        assert!(!controller.view.status_line.contains("Loading"));
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        controller.show_screen(Screen::Search);
    }
    controller.show_screen(Screen::YandexMusic);
    controller.view.search_query.clear();
    controller.dispatch(UiAction::SubmitSearch);
    assert!(matches!(
        requests.try_recv().unwrap(),
        YandexMusicWorkerRequest::Bootstrap { .. }
    ));
    controller.submit_yandex_music_search("fixture".to_owned());
    assert!(
        matches!(requests.try_recv().unwrap(), YandexMusicWorkerRequest::Search { query, .. } if query == "fixture")
    );
    controller.config.persistence.save_playback_history = true;
    controller.show_screen(Screen::Search);
    controller.show_screen(Screen::YandexMusic);
    assert!(matches!(
        requests.try_recv().unwrap(),
        YandexMusicWorkerRequest::Bootstrap { .. }
    ));
}

/// Live Yandex results survive disabling history and subsequent tab changes.
#[cfg(feature = "yandex-music")]
#[test]
fn disabling_history_retains_live_yandex_results() {
    let (_directory, mut controller) = controller();
    let requests = capture_yandex_requests(&mut controller);
    controller.config.persistence.save_playback_history = true;
    controller.yandex_music_rows = vec![YandexMusicRow::Track(Box::new(
        yandex_music_track_fixture(),
    ))];
    controller.show_screen(Screen::YandexMusic);
    let rows = controller.view.rows.clone();
    controller.config.persistence.save_playback_history = false;
    controller.show_screen(Screen::Search);
    controller.show_screen(Screen::YandexMusic);
    assert_eq!(controller.view.rows, rows);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
}
