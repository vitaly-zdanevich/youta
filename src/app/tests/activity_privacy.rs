//! Private playback keeps previous durable activity intact through lifecycle barriers.

use super::*;

/// Captures all automatic state documents, including interrupted playback recovery.
fn automatic_documents(config: &Config) -> Vec<(PathBuf, Vec<u8>)> {
    [
        config.state_dir().join("progress.toml"),
        config.state_dir().join("history.toml"),
        config.state_dir().join("statistics.toml"),
        config.runtime_dir().join("session.toml"),
        config.runtime_dir().join("playback-checkpoint.toml"),
        config.cache_dir().join("searches.toml"),
        config.cache_dir().join("providers.toml"),
    ]
    .into_iter()
    .map(|path| {
        let contents = std::fs::read(&path).expect("existing automatic activity document");
        (path, contents)
    })
    .collect()
}

/// Seeds meaningful previous activity so preservation cannot pass with only empty files.
fn seed_previous_activity(config: &Config, item: &QueueItem) {
    let store = StateStore::open(config).expect("durable fixture state");
    let mut progress = PlaybackProgress::new(item.media.id.clone(), Some(120), 1);
    progress.record_position(12, 1);
    store.upsert_progress(&progress).unwrap();
    store.add_listen_seconds(&item.media.id.source, 9).unwrap();
    store
        .insert_history(&HistoryEntry {
            id: 0,
            media_id: item.media.id.clone(),
            title: "Previous saved item".to_owned(),
            replay_locator: Some(item.media.webpage_url.to_string()),
            started_at: 1,
            last_played_at: 1,
            position_seconds: 12,
            duration_seconds: Some(120),
            finished: false,
        })
        .unwrap();
    store
        .save_session(
            &SessionState {
                search_text: "previous saved query".to_owned(),
                ..SessionState::default()
            },
            1,
        )
        .unwrap();
    store
        .save_youtube_search(
            &SavedYouTubeSearch {
                request: SearchRequest::new("previous saved query", SearchTarget::Videos),
                results: Vec::new(),
                next_page: None,
            },
            1,
        )
        .unwrap();
    store
        .put_cached_wikidata(&crate::persistence::CachedWikidataLookup {
            property_id: "P1651".to_owned(),
            external_id: "dQw4w9WgXcQ".to_owned(),
            items: Vec::new(),
            fetched_at: 1,
            expires_at: i64::MAX,
        })
        .unwrap();
}

/// Applies the actual preference action rather than bypassing its transition handling.
fn set_saving(controller: &mut AppController, enabled: bool) {
    assert_ne!(controller.config.persistence.save_playback_history, enabled);
    controller.dispatch(UiAction::OpenPreferences);
    controller.dispatch(UiAction::TogglePlaybackHistorySaving);
    controller.dispatch(UiAction::SubmitPreferences);
    assert_eq!(controller.config.persistence.save_playback_history, enabled);
    assert!(controller.view.preferences_popup.is_none());
    assert!(controller.view.error_popup.is_none());
}

/// Accepted playback, checkpoints, session autosave, and shutdown never rewrite old activity.
#[test]
fn private_playback_and_shutdown_preserve_existing_activity_bytes() {
    let directory = crate::test_support::canonical_tempdir("private controller playback");
    let mut config = Config::for_dir(directory.path().join("config"));
    let previous = fixture_direct_item("previous");
    seed_previous_activity(&config, &previous);
    {
        let store = StateStore::open(&config).unwrap();
        let mut progress = store.progress(&previous.media.id).unwrap().unwrap();
        progress.record_position(19, 2);
        store
            .checkpoint_playback(&progress, &previous.media.id.source, 3)
            .unwrap();
    }
    let before = automatic_documents(&config);
    config.persistence.save_playback_history = false;
    let (factory, playback, statuses, events) = mock_playback_factory([], []);
    let store = StateStore::open(&config).unwrap();
    let mut controller = AppController::new(config.clone(), store, None, Some(factory));
    let mut private = fixture_direct_item("private-playback");
    private.media.duration_seconds = Some(120);
    let private_id = private.media.id.clone();
    controller.play_queue_item(private, false);
    events
        .lock()
        .unwrap()
        .extend([PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted]);
    statuses.lock().unwrap().push_back(PlaybackStatus {
        position: Duration::from_secs(80),
        duration: Some(Duration::from_secs(120)),
        idle: false,
        paused: false,
        ..PlaybackStatus::default()
    });
    controller.update_player();
    assert_eq!(controller.playback_phase, PlaybackPhase::Playing);
    assert_eq!(playback.lock().unwrap().played.len(), 1);
    controller.account_listen_time(Duration::from_secs(42));
    assert!(controller.checkpoint_position());
    assert!(
        controller
            .store
            .listened_seconds(&private_id.source)
            .unwrap()
            >= 54
    );
    assert_eq!(
        controller
            .store
            .progress(&private_id)
            .unwrap()
            .unwrap()
            .position_seconds,
        80
    );
    controller.view.search_query = "private current query".to_owned();
    assert!(controller.save_session());
    controller.shutdown_for_exit().expect("private shutdown");
    drop(controller);
    assert_eq!(automatic_documents(&config), before);
    let reopened = StateStore::open(&config).unwrap();
    assert!(reopened.progress(&private_id).unwrap().is_none());
    assert_eq!(
        reopened
            .progress(&previous.media.id)
            .unwrap()
            .unwrap()
            .position_seconds,
        19
    );
    assert_eq!(reopened.history(false, 10).unwrap().len(), 1);
}

/// Re-enabling saves only newly accrued time and starts from the durable progress baseline.
#[test]
fn reenabling_activity_drops_private_listening_and_cached_progress() {
    let directory = crate::test_support::canonical_tempdir("private controller transition");
    let config = Config::for_dir(directory.path().join("config"));
    let mut item = fixture_direct_item("continuing-playback");
    item.media.duration_seconds = Some(120);
    seed_previous_activity(&config, &item);
    let store = StateStore::open(&config).unwrap();
    let (factory, _, statuses, events) = mock_playback_factory([], []);
    let mut controller = AppController::new(config.clone(), store, None, Some(factory));
    set_saving(&mut controller, false);
    controller.play_queue_item(item.clone(), false);
    events
        .lock()
        .unwrap()
        .extend([PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted]);
    statuses.lock().unwrap().push_back(PlaybackStatus {
        position: Duration::from_secs(80),
        duration: Some(Duration::from_secs(120)),
        idle: false,
        paused: false,
        ..PlaybackStatus::default()
    });
    controller.update_player();
    assert_eq!(controller.playback_phase, PlaybackPhase::Playing);
    assert!(!controller.view.playback.paused);
    controller.account_listen_time(Duration::from_secs(40));
    assert!(controller.checkpoint_position());
    assert_eq!(
        controller
            .current_playback_progress
            .as_ref()
            .unwrap()
            .position_seconds,
        80
    );
    controller.account_listen_time(Duration::from_millis(12_750));
    assert!(controller.unflushed_listen_time >= Duration::from_secs(12));
    set_saving(&mut controller, true);
    assert_eq!(controller.unflushed_listen_time, Duration::ZERO);
    assert_eq!(
        controller
            .current_playback_progress
            .as_ref()
            .unwrap()
            .position_seconds,
        12
    );
    assert_eq!(
        controller
            .store
            .listened_seconds(&item.media.id.source)
            .unwrap(),
        9
    );
    controller.view.playback.position = Duration::from_secs(25);
    controller.account_listen_time(Duration::from_secs(4));
    assert!(controller.persist_position());
    controller.shutdown_for_exit().unwrap();
    drop(controller);
    let reopened = StateStore::open(&config).unwrap();
    assert_eq!(
        reopened
            .progress(&item.media.id)
            .unwrap()
            .unwrap()
            .position_seconds,
        25
    );
    assert_eq!(
        reopened.listened_seconds(&item.media.id.source).unwrap(),
        13
    );
    assert_eq!(
        reopened.history(false, 10).unwrap().len(),
        1,
        "the private start must not become a later history entry"
    );
}
