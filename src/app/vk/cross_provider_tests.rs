//! Offline regression coverage for playback ownership across provider workers.

use super::*;

/// Initializes navigation with no credentials, network requests or playback process.
fn controller() -> AppController {
    let mut app = AppController::new(
        Config::for_dir("/tmp/youta-vk-cross-provider-fixture"),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    app.show_screen(Screen::Vk);
    app
}

/// Supplies public metadata with a fixed native VK identity.
fn track() -> VkTrack {
    VkTrack::fixture(
        VkAudioId::new(7, 11).unwrap(),
        "Fixture track",
        "Fixture artist",
    )
}

/// Occupies the VK request lane until the test explicitly publishes a completion.
fn hold_worker(app: &mut AppController, job: Job) -> Sender<Completion> {
    let path = PathBuf::from("/tmp/youta-vk-cross-provider-fixture-cookies");
    app.config.providers.vk_cookies_file = Some(path.clone());
    app.vk.cookie_file = Some(path.clone());
    let (sender, response) = bounded(1);
    app.vk.worker = Some(Worker {
        job,
        cookie_file: path,
        cookie_generation: 0,
        response,
        thread: thread::spawn(|| {}),
    });
    sender
}

/// Direct Yandex selection must revoke an older VK resolver before either completes.
#[cfg(feature = "yandex-music")]
#[test]
fn newer_yandex_intent_revokes_vk_and_ignores_its_late_completion() {
    let mut app = controller();
    app.yandex_music_stopping
        .store(true, AtomicOrdering::Release);
    app.yandex_music_requests.take();
    if let Some(worker) = app.yandex_music_thread.take() {
        worker.join().unwrap();
    }
    let (requests, observed_requests) = bounded(1);
    app.yandex_music_requests = Some(requests);

    let vk_track = track();
    let generation = app.vk.playback_generation;
    let completion = hold_worker(
        &mut app,
        Job::Playback(Box::new(PlaybackJob {
            generation,
            item: queue_item(&vk_track),
            queued: false,
            origin: None,
        })),
    );
    assert!(app.vk_playback_pending());

    let mut yandex_item = queue_item(&vk_track);
    yandex_item.media.id = MediaId::new(SourceKind::YandexMusic, "501");
    yandex_item.media.webpage_url =
        url::Url::parse("https://music.yandex.ru/album/10/track/501").unwrap();
    yandex_item.playback_location = yandex_item.media.webpage_url.to_string();
    let yandex_track = yandex_music_track_from_queue_item(&yandex_item);
    app.request_yandex_music_playback(yandex_track, yandex_item, false, None);
    assert!(matches!(
        observed_requests.try_recv().unwrap(),
        YandexMusicWorkerRequest::Resolve { .. }
    ));
    assert_ne!(
        app.vk.playback_generation, generation,
        "the newer direct Yandex request must invalidate the older VK worker"
    );
    assert!(!app.vk_playback_pending());

    let location = crate::providers::vk::decode_audio_url(
        "https://fixture.vkuseraudio.net/audio.m3u8?extra=obsolete-fixture",
        1,
    )
    .unwrap();
    assert!(
        completion
            .send(Completion {
                client: None,
                result: Ok(Reply::Playback(Box::new(vk_track), location)),
            })
            .is_ok()
    );
    app.poll_vk_worker();
    assert!(app.vk.worker.is_none());
    assert!(app.current_media.is_none());
    assert!(
        app.pending_yandex_music_playback.is_some(),
        "obsolete VK completion must preserve the newer Yandex intent"
    );
}

/// Selecting VK must immediately revoke an older BBC request while the VK lane is busy.
#[cfg(feature = "bbc-radio")]
#[test]
fn newer_vk_intent_revokes_an_older_bbc_resolver_before_starting_http() {
    let mut app = controller();
    let vk_item = queue_item(&track());
    let _hold_reply = hold_worker(
        &mut app,
        Job::Search(SearchJob {
            generation: 0,
            query: VkQuery::Search("old fixture query".to_owned()),
            kind: VkSearchKind::Tracks,
            cursor: None,
        }),
    );
    let mut bbc_item = vk_item.clone();
    bbc_item.media.id = MediaId::new(SourceKind::Radio, "bbc-radio-1");
    bbc_item.media.kind = MediaKind::LiveStream;
    bbc_item.media.webpage_url =
        url::Url::parse("https://www.bbc.co.uk/sounds/play/live:bbc_radio_one").unwrap();
    bbc_item.playback_location = bbc_item.media.webpage_url.to_string();
    let generation = app.bbc_playback_generation;
    app.pending_bbc_playback = Some(PendingBbcPlayback {
        generation,
        item: bbc_item,
        queue_cursor_already_positioned: false,
        origin: None,
    });

    app.request_vk_playback(vk_item, false, None);
    assert!(
        app.pending_bbc_playback.is_none(),
        "the newer VK request must revoke BBC before waiting for its HTTP lane"
    );
    assert_ne!(app.bbc_playback_generation, generation);
    assert!(app.vk.pending_playback.is_some());
    assert!(app.vk.worker.is_some());
    assert!(
        app.vk.client.is_none(),
        "the fixture must not import any credentials"
    );
}

/// Rewinding the active LIVE stream must revoke an older pending VK selection.
#[cfg(feature = "yt-dlp")]
#[test]
fn newer_live_start_revokes_vk_and_ignores_its_late_completion() {
    let mut app = controller();
    let mut live_item = queue_item(&track());
    live_item.media.id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
    live_item.media.kind = MediaKind::LiveStream;
    live_item.media.duration_seconds = None;
    live_item.media.webpage_url =
        url::Url::parse("https://www.youtube.com/watch?v=abcdefghijk").unwrap();
    live_item.playback_location = live_item.media.webpage_url.to_string();
    let live_id = live_item.media.id.clone();
    app.playback_queue.items = vec![live_item];
    app.playback_queue.current_index = Some(0);
    app.current_media = Some(live_id.clone());
    app.view.playing_media_id = Some(live_id.clone());
    app.view.playback.idle = false;
    app.view.playback.live = true;
    app.playback_phase = PlaybackPhase::Playing;
    let (generation, completion) = hold_vk_playback(&mut app);
    let (release, wait) = bounded::<()>(1);
    let (started, starts) = bounded(1);
    app.live_start.prepare = Arc::new(move |_, _, _| {
        let _ = started.send(());
        let _ = wait.recv_timeout(Duration::from_secs(3));
        Err("Fixture preparation released".to_owned())
    });

    app.dispatch(UiAction::SeekLiveStart);
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(app.live_start.preparing());
    assert_ne!(
        app.vk.playback_generation, generation,
        "the newer LIVE-start request must revoke the older VK worker"
    );
    assert!(!app.vk_playback_pending());
    let location = crate::providers::vk::decode_audio_url(
        "https://fixture.vkuseraudio.net/audio.m3u8?extra=obsolete-fixture",
        1,
    )
    .unwrap();
    assert!(
        completion
            .send(Completion {
                client: None,
                result: Ok(Reply::Playback(Box::new(track()), location)),
            })
            .is_ok()
    );
    app.poll_vk_worker();
    assert!(app.vk.worker.is_none());
    assert_eq!(app.current_media.as_ref(), Some(&live_id));
    assert!(
        app.live_start.preparing(),
        "obsolete VK completion must not cancel the newer LIVE-start preparation"
    );
    assert!(app.vk.retained.is_empty());
    release.send(()).unwrap();
    app.shutdown_live_start();
}

/// Replaces the shared network worker with a bounded, test-owned request channel.
fn capture_provider_requests(app: &mut AppController) -> Receiver<ProviderRequest> {
    if let Some(sender) = app.provider_requests.take() {
        let _ = sender.send(ProviderRequest::Shutdown);
    }
    if let Some(worker) = app.provider_thread.take() {
        worker.join().unwrap();
    }
    let (sender, receiver) = bounded(1);
    app.provider_requests = Some(sender);
    receiver
}

/// Starts an unresolved VK intent without importing credentials or contacting VK.
fn hold_vk_playback(app: &mut AppController) -> (u64, Sender<Completion>) {
    let generation = app.vk.playback_generation;
    let completion = hold_worker(
        app,
        Job::Playback(Box::new(PlaybackJob {
            generation,
            item: queue_item(&track()),
            queued: false,
            origin: None,
        })),
    );
    assert!(app.vk_playback_pending());
    (generation, completion)
}

/// Verifies cancellation before publishing a stale result that must remain unaccepted.
fn assert_vk_revoked_and_late_completion_ignored(
    app: &mut AppController,
    generation: u64,
    completion: Sender<Completion>,
) {
    assert_ne!(
        app.vk.playback_generation, generation,
        "the newer asynchronous playback request must invalidate the older VK worker"
    );
    assert!(!app.vk_playback_pending());
    let location = crate::providers::vk::decode_audio_url(
        "https://fixture.vkuseraudio.net/audio.m3u8?extra=obsolete-fixture",
        1,
    )
    .unwrap();
    assert!(
        completion
            .send(Completion {
                client: None,
                result: Ok(Reply::Playback(Box::new(track()), location)),
            })
            .is_ok()
    );
    app.poll_vk_worker();
    assert!(app.vk.worker.is_none());
    assert!(app.current_media.is_none());
    assert!(
        app.vk.retained.is_empty(),
        "an ignored completion must not admit even its private replay metadata"
    );
}

/// New manual Tracker preparation supersedes VK before the module is downloaded.
#[cfg(feature = "tracker-music")]
#[test]
fn newer_tracker_preparation_revokes_vk_and_ignores_its_late_completion() {
    let mut app = controller();
    let requests = capture_provider_requests(&mut app);
    let (generation, completion) = hold_vk_playback(&mut app);
    app.show_screen(Screen::TrackerMusic);
    app.tracker_results = vec![TrackerItem {
        source: "Fixture archive".to_owned(),
        title: "Fixture module".to_owned(),
        subtitle: String::new(),
        webpage_url: url::Url::parse("https://mods.example/fixture").unwrap(),
        download_url: url::Url::parse("https://mods.example/fixture.mod").ok(),
        expected_format: Some("mod".to_owned()),
        access: TrackerAccess::DirectModule,
        prepared_path: None,
        insecure_transport: false,
    }];

    assert!(app.prepare_tracker_item(0, TrackerPreparationOwner::Manual));
    assert!(matches!(
        requests.try_recv().unwrap(),
        ProviderRequest::PrepareTracker { .. }
    ));
    assert_vk_revoked_and_late_completion_ignored(&mut app, generation, completion);
    assert!(matches!(
        app.pending_tracker_preparation
            .as_ref()
            .map(|pending| &pending.owner),
        Some(TrackerPreparationOwner::Manual)
    ));
}

/// First-class History resolution supersedes VK while its stable page is loading.
#[test]
fn newer_history_replay_revokes_vk_and_ignores_its_late_completion() {
    let mut app = controller();
    let requests = capture_provider_requests(&mut app);
    let (generation, completion) = hold_vk_playback(&mut app);
    let page = url::Url::parse("https://podcasts.apple.com/us/podcast/fixture-show/id2002?i=4004")
        .unwrap();
    let entry = HistoryEntry {
        id: 1,
        media_id: MediaId::new(SourceKind::ApplePodcasts, "4004"),
        title: "Fixture episode".to_owned(),
        replay_locator: Some(page.to_string()),
        started_at: 1,
        last_played_at: 2,
        position_seconds: 0,
        duration_seconds: Some(120),
        finished: false,
    };
    app.show_screen(Screen::History);
    app.history_entries = vec![HistoryListEntry {
        entry: entry.clone(),
        local_removed: false,
    }];

    app.resolve_history_selection(entry.clone(), page);
    assert!(matches!(
        requests.try_recv().unwrap(),
        ProviderRequest::ResolveApple { .. }
    ));
    assert_vk_revoked_and_late_completion_ignored(&mut app, generation, completion);
    assert_eq!(
        app.pending_history_replay
            .as_ref()
            .map(|pending| pending.entry.id),
        Some(entry.id)
    );
}

/// First-class playlist resolution supersedes VK before the selected item resolves.
#[test]
fn newer_playlist_replay_revokes_vk_and_ignores_its_late_completion() {
    let mut app = controller();
    let requests = capture_provider_requests(&mut app);
    let (generation, completion) = hold_vk_playback(&mut app);
    let page = url::Url::parse("https://podcasts.apple.com/us/podcast/fixture-show/id2002?i=4004")
        .unwrap();
    let owner = PlaylistReplaySelection {
        playlist_id: "fixture-playlist".to_owned(),
        index: 0,
        entry: PlaylistEntry {
            media: PlaylistMediaSnapshot {
                id: MediaId::new(SourceKind::ApplePodcasts, "4004"),
                kind: MediaKind::Audio,
                title: "Fixture episode".to_owned(),
                creator: None,
                description: None,
                webpage_url: page.clone(),
                thumbnail_url: None,
                duration_seconds: Some(120),
                replay_locator: page.to_string(),
            },
            segment: None,
            added_at: 1,
        },
    };
    app.show_screen(Screen::Playlists);
    app.playlists_route = PlaylistsRoute::Entries {
        playlist_id: owner.playlist_id.clone(),
    };
    app.active_playlist = Some(Playlist {
        id: owner.playlist_id.clone(),
        name: "Fixture playlist".to_owned(),
        description: None,
        entries: vec![owner.entry.clone()],
    });

    app.resolve_playlist_entry(owner.clone(), None);
    assert!(matches!(
        requests.try_recv().unwrap(),
        ProviderRequest::ResolveApple { .. }
    ));
    assert_vk_revoked_and_late_completion_ignored(&mut app, generation, completion);
    assert_eq!(
        app.pending_playlist_replay
            .as_ref()
            .map(|pending| &pending.owner),
        Some(&owner)
    );
}
