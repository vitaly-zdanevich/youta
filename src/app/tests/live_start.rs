//! Offline controller regressions for original-broadcast playback ownership.

use super::*;
use crate::app::live_start::LiveStartGuard;

/// Loading placeholders cannot anchor elapsed time, even without a rewind cache.
#[test]
fn live_elapsed_without_cache_waits_for_active_timestamp_and_advances() {
    let (mut controller, _) = live_start_controller();
    controller.playback_phase = PlaybackPhase::Loading;
    let mut status = PlaybackStatus {
        idle: false,
        position: Duration::from_secs(999),
        ..PlaybackStatus::default()
    };
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, Some(0));
    assert_eq!(controller.youtube_live_time_origin, None);
    controller.playback_phase = PlaybackPhase::Playing;
    status.position = Duration::ZERO;
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.youtube_live_time_origin, None);
    status.position = Duration::from_secs(1_000);
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, Some(0));
    status.position = Duration::from_secs(1_013);
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, Some(13));
    assert_eq!(status.position, Duration::ZERO);
    assert_eq!(status.duration, None);
}

/// Moving cache boundaries must not reset the elapsed clock or alter seek projection.
#[test]
fn live_elapsed_is_stable_when_the_rewind_cache_slides() {
    let (mut controller, _) = live_start_controller();
    let mut status = PlaybackStatus {
        idle: false,
        position: Duration::from_secs(1_000),
        buffered_ranges: vec![BufferedRange {
            start: Duration::from_secs(900),
            end: Duration::from_secs(1_020),
        }],
        ..PlaybackStatus::default()
    };
    controller.normalize_playback_status(&mut status);
    assert_eq!(status.position, Duration::from_secs(100));
    status.position = Duration::from_secs(1_013);
    status.buffered_ranges = vec![BufferedRange {
        start: Duration::from_secs(1_005),
        end: Duration::from_secs(1_035),
    }];
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, Some(13));
    assert_eq!(status.position, Duration::from_secs(8));
    assert_eq!(status.duration, Some(Duration::from_secs(30)));
}

/// Paused snapshots retain media-time elapsed, and a replacement/reset releases it.
#[test]
fn live_elapsed_pause_and_replacement_preserve_clock_ownership() {
    let (mut controller, _) = live_start_controller();
    let mut status = PlaybackStatus {
        idle: false,
        position: Duration::from_secs(1_000),
        ..PlaybackStatus::default()
    };
    controller.normalize_playback_status(&mut status);
    status.position = Duration::from_secs(1_013);
    status.paused = true;
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, Some(13));
    status.position = Duration::from_secs(1_013);
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, Some(13));
    status.position = Duration::from_secs(1_015);
    status.paused = false;
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, Some(15));
    controller.play_queue_item(fixture_direct_item("Finite replacement"), false);
    assert_eq!(controller.view.live_elapsed_seconds, None);
    assert_eq!(controller.youtube_live_time_origin, None);
    controller.reset_playback_state();
    assert_eq!(controller.view.live_elapsed_seconds, None);
}

/// Accepted original-broadcast audio starts at zero rather than the first snapshot.
#[test]
fn live_elapsed_from_start_uses_the_broadcast_zero_origin() {
    let (mut controller, _) = live_start_controller();
    controller.live_start.accepting_input = true;
    let item = controller.playback_queue.current().unwrap().clone();
    controller.play_queue_item(item, false);
    controller.live_start.accepting_input = false;
    assert_eq!(controller.youtube_live_time_origin, Some(Duration::ZERO));
    assert_eq!(controller.view.live_elapsed_seconds, Some(0));
    controller.playback_phase = PlaybackPhase::Playing;
    let mut status = PlaybackStatus {
        idle: false,
        position: Duration::from_secs(13),
        ..PlaybackStatus::default()
    };
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, Some(13));
    let mut replacement = controller.playback_queue.current().unwrap().clone();
    replacement.media.id.source = SourceKind::Radio;
    controller.play_queue_item(replacement, false);
    controller.normalize_playback_status(&mut status);
    assert_eq!(controller.view.live_elapsed_seconds, None);
    assert_eq!(controller.youtube_live_time_origin, None);
}

/// An in-memory transport exposes no network or extractor process to these tests.
struct MockLiveStartGuard {
    url: url::Url,
    stopped: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
}

impl LiveStartGuard for MockLiveStartGuard {
    fn url(&self) -> &url::Url {
        &self.url
    }
    fn set_worker_waker(&self, _waker: Option<std::task::Waker>) {}
    fn take_error(&mut self) -> Option<String> {
        self.failure.lock().unwrap().take()
    }
    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

/// Installs one live queue identity while keeping optional provider enrichment offline.
fn live_start_controller() -> (AppController, Arc<Mutex<MockPlaybackState>>) {
    let (mut controller, state, _, _) = controller_with_mock_lifecycle([], []);
    let _requests = capture_controller_provider_requests(&mut controller);
    controller.provider_requests = None;
    controller.diagnostic_helpers_cache = Some(Vec::new());
    let mut item = fixture_youtube_item("Original live broadcast");
    item.media.kind = MediaKind::LiveStream;
    item.media.duration_seconds = None;
    controller.play_queue_item(item, false);
    controller.view.playing_media_id = controller.current_media.clone();
    controller.playback_phase = PlaybackPhase::Playing;
    controller.view.playback.live = true;
    controller.view.playback.paused = false;
    controller.view.playback.buffering = false;
    controller.view.playback.position = Duration::from_secs(50);
    state.lock().unwrap().played.clear();
    state.lock().unwrap().commands.clear();
    (controller, state)
}

/// Polls only the controller's async completions, never the backend or provider network.
fn poll_live_start_until(controller: &mut AppController, done: impl Fn(&AppController) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !done(controller) {
        assert!(
            Instant::now() < deadline,
            "live-start mock completion timed out"
        );
        controller.poll_live_start();
        thread::sleep(Duration::from_millis(1));
    }
}

/// Preparation leaves old audio alone, then replaces only its transient player input.
#[test]
fn live_start_ready_preserves_queue_identity_and_autoplay_origin() {
    let (mut controller, player) = live_start_controller();
    let stopped = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(Mutex::new(None));
    let (ready, wait) = bounded(1);
    let (started, starts) = bounded(1);
    let guard_stopped = Arc::clone(&stopped);
    let guard_failure = Arc::clone(&failure);
    controller.live_start.prepare = Arc::new(move |_, source, _| {
        assert_eq!(source.host_str(), Some("www.youtube.com"));
        started.send(()).unwrap();
        wait.recv().unwrap();
        Ok(Box::new(MockLiveStartGuard {
            url: url::Url::parse("http://127.0.0.1:54321/private-live-start").unwrap(),
            stopped: Arc::clone(&guard_stopped),
            failure: Arc::clone(&guard_failure),
        }))
    });
    let queue = controller.playback_queue.clone();
    let origin = Some(AutoplayOrigin::YouTube {
        generation: 12,
        index: 4,
    });
    controller.current_autoplay_origin = origin.clone();
    controller.dispatch(UiAction::SeekLiveStart);
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(controller.view.playback_preparing);
    assert_eq!(controller.view.playing_media_id, controller.current_media);
    assert!(player.lock().unwrap().played.is_empty());
    assert!(player.lock().unwrap().commands.is_empty());
    assert_eq!(controller.view.playback.position, Duration::from_secs(50));
    ready.send(()).unwrap();
    poll_live_start_until(&mut controller, |controller| {
        controller.live_start.active.is_some()
    });
    let input = player.lock().unwrap().played.last().cloned().unwrap();
    assert_eq!(input.location, "http://127.0.0.1:54321/private-live-start");
    assert!(input.bypass_ytdl);
    assert_eq!(input.start_at, Duration::ZERO);
    assert_eq!(input.title.as_deref(), Some("Original live broadcast"));
    assert!(input.cache_identity.is_none());
    assert!(
        input.cache_fallback_url.is_none(),
        "failure must not silently return to the live edge"
    );
    assert_eq!(controller.playback_queue, queue);
    assert_eq!(controller.current_autoplay_origin, origin);
    assert_eq!(
        controller
            .pending_history
            .as_ref()
            .unwrap()
            .replay_locator
            .as_deref(),
        Some(queue.current().unwrap().playback_location.as_str())
    );
    assert!(!stopped.load(Ordering::Acquire));
    controller.shutdown_live_start();
    assert!(stopped.load(Ordering::Acquire));
}

/// Unavailable initial audio retains the active broadcast and exposes no signed helper URL.
#[test]
fn live_start_prepare_failure_keeps_old_audio_without_cached_fallback() {
    let (mut controller, player) = live_start_controller();
    controller.live_start.prepare = Arc::new(|_, _, _| {
        Err("beginning unavailable at https://media.invalid/?signature=secret".to_owned())
    });
    controller.dispatch(UiAction::SeekLiveStart);
    poll_live_start_until(&mut controller, |controller| {
        controller.view.error_popup.is_some()
    });
    assert!(player.lock().unwrap().played.is_empty());
    assert!(player.lock().unwrap().commands.is_empty());
    assert_eq!(controller.playback_phase, PlaybackPhase::Playing);
    assert_eq!(controller.view.playback.position, Duration::from_secs(50));
    let popup = controller.view.error_popup.as_ref().unwrap();
    assert!(popup.summary.as_deref().unwrap().contains("beginning"));
    assert!(!popup.report.contains("media.invalid"));
    assert!(!popup.report.contains("signature"));
    assert!(!popup.reportable);
}

/// A late successful helper result cannot replace a newly selected media item.
#[test]
fn live_start_replaced_owner_discards_and_stops_late_stream() {
    let (mut controller, player) = live_start_controller();
    let stopped = Arc::new(AtomicBool::new(false));
    let guard_stopped = Arc::clone(&stopped);
    let (release, wait) = bounded(1);
    let (started, starts) = bounded(1);
    controller.live_start.prepare = Arc::new(move |_, _, _| {
        started.send(()).unwrap();
        wait.recv().unwrap();
        Ok(Box::new(MockLiveStartGuard {
            url: url::Url::parse("http://127.0.0.1:54321/obsolete").unwrap(),
            stopped: Arc::clone(&guard_stopped),
            failure: Arc::new(Mutex::new(None)),
        }))
    });
    controller.dispatch(UiAction::SeekLiveStart);
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut replacement = fixture_youtube_item("Replacement");
    replacement.media.id.external_id = "9bZkp7q19f0".into();
    replacement.playback_location = "https://www.youtube.com/watch?v=9bZkp7q19f0".into();
    controller.play_queue_item(replacement, false);
    let replaced_owner = controller.current_media.clone();
    release.send(()).unwrap();
    poll_live_start_until(&mut controller, |_| stopped.load(Ordering::Acquire));
    assert_eq!(controller.current_media, replaced_owner);
    assert!(controller.live_start.active.is_none());
    assert_eq!(player.lock().unwrap().played.len(), 1);
}

/// Later transport failures are explicit and cannot retry through the canonical live edge.
#[test]
fn live_start_transport_failure_stops_guard_and_fails_closed() {
    let (mut controller, player) = live_start_controller();
    let stopped = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(Mutex::new(None));
    let guard_stopped = Arc::clone(&stopped);
    let guard_failure = Arc::clone(&failure);
    controller.live_start.prepare = Arc::new(move |_, _, _| {
        Ok(Box::new(MockLiveStartGuard {
            url: url::Url::parse("http://127.0.0.1:54321/failing-stream").unwrap(),
            stopped: Arc::clone(&guard_stopped),
            failure: Arc::clone(&guard_failure),
        }))
    });
    controller.dispatch(UiAction::SeekLiveStart);
    poll_live_start_until(&mut controller, |controller| {
        controller.live_start.active.is_some()
    });
    *failure.lock().unwrap() =
        Some("download failed https://media.invalid/?token=secret".to_owned());
    controller.poll_live_start();
    assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
    assert!(controller.current_media.is_none());
    assert_eq!(player.lock().unwrap().played.len(), 1);
    assert!(
        !controller
            .view
            .error_popup
            .as_ref()
            .unwrap()
            .report
            .contains("token=secret")
    );
    controller.shutdown_live_start();
    assert!(stopped.load(Ordering::Acquire));
}

/// A seek dispatched before the next tick cannot turn a failed helper into EOF autoplay.
#[test]
fn live_start_failed_helper_eof_before_tick_cannot_advance_the_queue() {
    let (mut controller, player, _, events) = controller_with_mock_lifecycle([], []);
    let _requests = capture_controller_provider_requests(&mut controller);
    controller.provider_requests = None;
    controller.diagnostic_helpers_cache = Some(Vec::new());
    let mut item = fixture_youtube_item("Failed live stream");
    item.media.kind = MediaKind::LiveStream;
    item.media.duration_seconds = None;
    controller.play_queue_item(item, false);
    controller.view.playing_media_id = controller.current_media.clone();
    controller.playback_phase = PlaybackPhase::Playing;
    player.lock().unwrap().played.clear();
    let failure = Arc::new(Mutex::new(None));
    let guard_failure = Arc::clone(&failure);
    controller.live_start.prepare = Arc::new(move |_, _, _| {
        Ok(Box::new(MockLiveStartGuard {
            url: url::Url::parse("http://127.0.0.1:54321/failed-eof").unwrap(),
            stopped: Arc::new(AtomicBool::new(false)),
            failure: Arc::clone(&guard_failure),
        }))
    });
    controller.dispatch(UiAction::SeekLiveStart);
    poll_live_start_until(&mut controller, |controller| {
        controller.live_start.active.is_some()
    });
    controller.playback_phase = PlaybackPhase::Playing;
    controller
        .playback_queue
        .items
        .push(fixture_youtube_item("Must not autoplay"));
    player.lock().unwrap().commands.clear();
    *failure.lock().unwrap() = Some("download failed https://media.invalid/?token=secret".into());
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::Ended(PlaybackEnd {
            reason: PlaybackEndReason::Eof,
            error: None,
            file_error: None,
            diagnostic: None,
        }));

    controller.dispatch(UiAction::SeekRelative(-5));

    assert_eq!(
        player.lock().unwrap().played.len(),
        1,
        "a failed helper must not load a successor"
    );
    assert!(player.lock().unwrap().commands.is_empty());
    assert!(controller.current_media.is_none());
    assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
    let popup = controller
        .view
        .error_popup
        .as_ref()
        .expect("explicit beginning failure");
    assert_eq!(popup.title, "Broadcast beginning is unavailable");
    assert!(!popup.report.contains("token=secret"));
    controller.shutdown_live_start();
}

/// An old EOF must select its successor before a ready helper can claim playback.
#[test]
fn live_start_old_eof_before_completion_does_not_replace_the_successor() {
    let (mut controller, player, _, events) = controller_with_mock_lifecycle([], []);
    let _requests = capture_controller_provider_requests(&mut controller);
    controller.provider_requests = None;
    let mut item = fixture_youtube_item("Ending live owner");
    item.media.kind = MediaKind::LiveStream;
    item.media.duration_seconds = None;
    controller.play_queue_item(item, false);
    controller.view.playing_media_id = controller.current_media.clone();
    controller.playback_phase = PlaybackPhase::Playing;
    player.lock().unwrap().played.clear();
    let stopped = Arc::new(AtomicBool::new(false));
    let guard_stopped = Arc::clone(&stopped);
    let (release, wait) = bounded(1);
    let (started, starts) = bounded(1);
    controller.live_start.prepare = Arc::new(move |_, _, _| {
        started.send(()).unwrap();
        wait.recv().unwrap();
        Ok(Box::new(MockLiveStartGuard {
            url: url::Url::parse("http://127.0.0.1:54321/ended-owner").unwrap(),
            stopped: Arc::clone(&guard_stopped),
            failure: Arc::new(Mutex::new(None)),
        }))
    });
    let successor = fixture_direct_item("queue-successor");
    let successor_id = successor.media.id.clone();
    let successor_location = successor.playback_location.clone();
    controller.playback_queue.items.push(successor);
    controller.dispatch(UiAction::SeekLiveStart);
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::Ended(PlaybackEnd {
            reason: PlaybackEndReason::Eof,
            error: None,
            file_error: None,
            diagnostic: None,
        }));
    release.send(()).unwrap();

    poll_live_start_until(&mut controller, |controller| {
        !controller.live_start.preparing()
    });

    let played = player.lock().unwrap().played.clone();
    assert_eq!(played.len(), 1, "only the old owner's successor may load");
    assert_eq!(played[0].location, successor_location);
    assert_eq!(controller.current_media.as_ref(), Some(&successor_id));
    assert!(controller.live_start.active.is_none());
    assert!(controller.view.error_popup.is_none());
    controller.shutdown_live_start();
    assert!(stopped.load(Ordering::Acquire));
}

/// A second zero prepares independently while the accepted from-start stream keeps playing.
#[test]
fn live_start_repeated_zero_retains_old_stream_until_new_audio_is_ready() {
    let (mut controller, player) = live_start_controller();
    let stopped = Arc::new(AtomicBool::new(false));
    let guard_stopped = Arc::clone(&stopped);
    let calls = Arc::new(AtomicUsize::new(0));
    let prepare_calls = Arc::clone(&calls);
    let (release, wait) = bounded(1);
    let (started, starts) = bounded(1);
    controller.live_start.prepare = Arc::new(move |_, _, _| {
        if prepare_calls.fetch_add(1, Ordering::AcqRel) == 1 {
            started.send(()).unwrap();
            wait.recv().unwrap();
        }
        Ok(Box::new(MockLiveStartGuard {
            url: url::Url::parse("http://127.0.0.1:54321/repeated-start").unwrap(),
            stopped: Arc::clone(&guard_stopped),
            failure: Arc::new(Mutex::new(None)),
        }))
    });
    controller.dispatch(UiAction::SeekLiveStart);
    poll_live_start_until(&mut controller, |controller| {
        controller.live_start.active.is_some()
    });
    controller.view.playing_media_id = controller.current_media.clone();
    controller.playback_phase = PlaybackPhase::Playing;
    controller.dispatch(UiAction::SeekLiveStart);
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    controller.poll_live_start();
    assert!(controller.live_start.active.is_some());
    assert!(!stopped.load(Ordering::Acquire));
    assert_eq!(controller.playback_phase, PlaybackPhase::Playing);
    assert_eq!(player.lock().unwrap().played.len(), 1);
    release.send(()).unwrap();
    poll_live_start_until(&mut controller, |_| {
        player.lock().unwrap().played.len() == 2
    });
    controller.shutdown_live_start();
    assert!(stopped.load(Ordering::Acquire));
}

/// Shutdown signals cancellation before joining a pending preparation worker.
#[test]
fn live_start_shutdown_cancels_and_joins_pending_preparation() {
    let (mut controller, player) = live_start_controller();
    let cancelled = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&cancelled);
    let (started, starts) = bounded(1);
    controller.live_start.prepare = Arc::new(move |_, _, cancellation| {
        started.send(()).unwrap();
        while !cancellation.is_cancelled() {
            thread::sleep(Duration::from_millis(1));
        }
        observed.store(true, Ordering::Release);
        Err("cancelled".into())
    });
    controller.dispatch(UiAction::SeekLiveStart);
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    controller.shutdown_live_start();
    assert!(cancelled.load(Ordering::Acquire));
    assert!(!controller.live_start.preparing());
    assert!(player.lock().unwrap().played.is_empty());
}

/// A deferred History resolver supersedes old zero intent before any new audio is ready.
#[test]
fn live_start_new_deferred_play_intent_discards_late_zero_result() {
    let (mut controller, player) = live_start_controller();
    let stopped = Arc::new(AtomicBool::new(false));
    let guard_stopped = Arc::clone(&stopped);
    let (release, wait) = bounded(1);
    let (started, starts) = bounded(1);
    controller.live_start.prepare = Arc::new(move |_, _, _| {
        started.send(()).unwrap();
        wait.recv().unwrap();
        Ok(Box::new(MockLiveStartGuard {
            url: url::Url::parse("http://127.0.0.1:54321/old-intent").unwrap(),
            stopped: Arc::clone(&guard_stopped),
            failure: Arc::new(Mutex::new(None)),
        }))
    });
    controller.dispatch(UiAction::SeekLiveStart);
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    let (requests, captured) = unbounded();
    controller.provider_requests = Some(requests);
    controller.view.screen = Screen::History;
    controller.history_entries = vec![HistoryListEntry {
        entry: HistoryEntry {
            id: 1,
            media_id: MediaId::new(SourceKind::ApplePodcasts, "4004"),
            title: "Deferred podcast".into(),
            replay_locator: Some(
                "https://podcasts.apple.com/us/podcast/fixture-show/id2002?i=4004".into(),
            ),
            started_at: 1,
            last_played_at: 2,
            position_seconds: 0,
            duration_seconds: Some(123),
            finished: false,
        },
        local_removed: false,
    }];
    controller.dispatch(UiAction::ActivateSelection);
    assert!(matches!(
        captured.recv_timeout(Duration::from_secs(3)).unwrap(),
        ProviderRequest::ResolveApple { .. }
    ));
    assert!(controller.pending_history_replay.is_some());
    assert!(!controller.live_start.preparing());
    release.send(()).unwrap();
    poll_live_start_until(&mut controller, |_| stopped.load(Ordering::Acquire));
    assert!(player.lock().unwrap().played.is_empty());
    assert!(controller.live_start.active.is_none());
    assert!(controller.pending_history_replay.is_some());
}
