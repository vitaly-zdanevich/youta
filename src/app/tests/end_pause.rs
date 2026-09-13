//! Regressions for retaining a finished `YouTube` timeline without another load.

use super::*;

#[test]
fn seeking_to_or_beyond_a_held_end_keeps_autoplay_release_available() {
    for command in [
        PlayerCommand::SeekRelative(5),
        PlayerCommand::SeekRelative(0),
        PlayerCommand::SeekPercent(100.0),
        PlayerCommand::SeekAbsolute(Duration::from_secs(42)),
        PlayerCommand::SeekAbsolute(Duration::from_secs(100)),
    ] {
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [near_end_status()],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = false;
        controller.play_queue_item(fixture_youtube_item("Forward at end"), false);
        controller.update_player();
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::EndOfFileHeld);
        controller.update_player();
        state.lock().unwrap().commands.clear();
        controller.seek_player_command(command);
        assert!(
            state.lock().unwrap().commands.is_empty(),
            "a no-op seek must retain mpv's EOF latch"
        );
        assert!(controller.view.playback.paused);
        assert_eq!(controller.view.playback.position, Duration::from_secs(42));
        controller.config.playback.autoplay = true;
        controller.update_player();
        assert_eq!(
            state.lock().unwrap().commands,
            [PlayerCommand::ReleaseEndOfFile]
        );
    }
}

/// A deterministic finite timeline just before its natural end.
fn near_end_status() -> PlaybackStatus {
    PlaybackStatus {
        idle: false,
        position: Duration::from_secs(40),
        duration: Some(Duration::from_secs(42)),
        paused: false,
        ..PlaybackStatus::default()
    }
}

/// A normal backend EOF emitted after a retained timeline is released.
fn released_eof() -> PlaybackEvent {
    PlaybackEvent::Ended(PlaybackEnd {
        reason: PlaybackEndReason::Eof,
        error: None,
        file_error: None,
        diagnostic: None,
    })
}

#[test]
fn finite_youtube_load_opts_into_native_end_pause_only() {
    for (source, kind, expected) in [
        (SourceKind::YouTube, MediaKind::Video, true),
        (SourceKind::YouTube, MediaKind::LiveStream, false),
        (SourceKind::RemoteFiles, MediaKind::Audio, false),
    ] {
        let (mut controller, state) = controller_with_mock_statuses([]);
        let mut item = fixture_youtube_item("End pause fixture");
        item.media.id.source = source;
        item.media.kind = kind;
        controller.play_queue_item(item, false);
        assert_eq!(state.lock().unwrap().played[0].keep_open, expected);
    }
}

#[test]
fn held_youtube_eof_preserves_timeline_progress_and_seek_context() {
    let (mut controller, state, _, events) = controller_with_mock_lifecycle(
        [near_end_status()],
        [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
    );
    controller.config.playback.autoplay = false;
    let item = fixture_youtube_item("Retained YouTube");
    let media = item.media.id.clone();
    controller.play_queue_item(item, false);
    controller.update_player();
    controller
        .seek_back
        .push_back((media.clone(), Duration::from_secs(7)));
    let queue_index = controller.playback_queue.current_index;
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::EndOfFileHeld);
    controller.update_player();

    assert_eq!(controller.current_media.as_ref(), Some(&media));
    assert_eq!(controller.view.playing_media_id.as_ref(), Some(&media));
    assert_eq!(controller.playback_queue.current_index, queue_index);
    assert_eq!(controller.view.playback.position, Duration::from_secs(42));
    assert_eq!(
        controller.view.playback.duration,
        Some(Duration::from_secs(42))
    );
    assert!(!controller.view.playback.idle);
    assert!(!controller.view.playback_end_releasing);
    assert!(controller.view.playback.paused);
    assert!(!controller.view.playback.buffering);
    assert!(controller.view.playback.seeking_available());
    assert_eq!(
        controller.seek_back.back(),
        Some(&(media.clone(), Duration::from_secs(7)))
    );
    assert_eq!(
        controller
            .store
            .progress(&media)
            .unwrap()
            .unwrap()
            .position_seconds,
        42
    );
    let listening = controller.unflushed_listen_time;
    controller.account_listen_time(Duration::from_secs(30));
    assert_eq!(
        controller.unflushed_listen_time, listening,
        "paused time is not listening time"
    );

    state.lock().unwrap().commands.clear();
    controller.dispatch(UiAction::SeekRelative(-5));
    controller.dispatch(UiAction::SeekRelative(2));
    controller.dispatch(UiAction::SeekPercent(50.0));
    let state = state.lock().unwrap();
    assert_eq!(
        state.played.len(),
        1,
        "seeking must not reload or resolve YouTube again"
    );
    assert_eq!(
        state.commands,
        [
            PlayerCommand::SeekRelative(-5),
            PlayerCommand::SeekRelative(2),
            PlayerCommand::SeekPercent(50.0)
        ]
    );
    assert!(controller.view.error_popup.is_none());
}

#[test]
fn arrow_input_can_seek_when_held_eof_arrives_before_the_next_tick() {
    let (mut controller, state, _, events) = controller_with_mock_lifecycle(
        [near_end_status()],
        [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
    );
    controller.config.playback.autoplay = false;
    controller.play_queue_item(fixture_youtube_item("Pending end"), false);
    controller.update_player();
    state.lock().unwrap().commands.clear();
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::EndOfFileHeld);
    controller.dispatch(UiAction::SeekRelative(-5));
    assert_eq!(
        state.lock().unwrap().commands,
        [PlayerCommand::SeekRelative(-5)]
    );
    assert_eq!(state.lock().unwrap().played.len(), 1);
    assert!(!controller.view.playback.paused);
    assert!(controller.view.error_popup.is_none());
}

#[test]
fn held_eof_releases_for_autoplay_repeat_or_an_explicit_queued_item() {
    for mode in ["autoplay", "repeat", "queue"] {
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [near_end_status()],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = mode == "autoplay";
        controller.playback_queue.repeat_one = mode == "repeat";
        controller.play_queue_item(fixture_youtube_item("First"), false);
        if mode == "queue" {
            controller.playback_queue.push(fixture_direct_item("Next"));
        }
        controller.update_player();
        state.lock().unwrap().commands.clear();
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::EndOfFileHeld);
        controller.dispatch(UiAction::SeekRelative(-5));
        assert_eq!(
            state.lock().unwrap().commands,
            [PlayerCommand::ReleaseEndOfFile],
            "{mode}: stale input must not seek a successor"
        );
        assert!(controller.view.playback_end_releasing);
        controller.dispatch(UiAction::SeekRelative(-5));
        assert_eq!(
            state.lock().unwrap().commands,
            [PlayerCommand::ReleaseEndOfFile],
            "{mode}: a second key must wait for the authoritative EOF event"
        );
        assert_eq!(
            state.lock().unwrap().played.len(),
            1,
            "only the ordinary EOF may advance the queue"
        );
        events.lock().unwrap().push_back(released_eof());
        controller.update_player();
        assert!(!controller.view.playback_end_releasing);
        let state = state.lock().unwrap();
        match mode {
            "repeat" => {
                assert_eq!(state.played.len(), 2);
                assert_eq!(state.played[1].title.as_deref(), Some("First"));
                assert_eq!(state.played[1].start_at, Duration::ZERO);
            }
            "queue" => {
                assert_eq!(state.played.len(), 2);
                assert_eq!(state.played[1].title.as_deref(), Some("Next"));
            }
            _ => assert!(
                controller.current_media.is_none(),
                "autoplay with no list must finish normally"
            ),
        }
        assert!(controller.view.error_popup.is_none());
    }
}

#[test]
fn enabling_continuation_after_a_held_eof_releases_the_timeline() {
    for repeat in [false, true] {
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [near_end_status()],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = false;
        controller.play_queue_item(fixture_youtube_item("Enable continuation"), false);
        controller.update_player();
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::EndOfFileHeld);
        controller.update_player();
        assert!(controller.view.playback.paused);
        state.lock().unwrap().commands.clear();
        if repeat {
            controller.dispatch(UiAction::ToggleRepeat);
        } else {
            controller.config.playback.autoplay = true;
        }
        controller.update_player();
        assert!(
            state
                .lock()
                .unwrap()
                .commands
                .contains(&PlayerCommand::ReleaseEndOfFile)
        );
    }
}

#[test]
fn held_eof_before_playback_started_is_not_a_successful_pause() {
    let (mut controller, state, _, events) = controller_with_mock_lifecycle([], []);
    controller.config.playback.autoplay = false;
    controller.play_queue_item(fixture_youtube_item("Not decoded"), false);
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::EndOfFileHeld);
    controller.update_player();
    assert!(
        state
            .lock()
            .unwrap()
            .commands
            .contains(&PlayerCommand::ReleaseEndOfFile)
    );
    events.lock().unwrap().push_back(released_eof());
    controller.update_player();
    assert!(controller.current_media.is_none());
    assert!(controller.view.error_popup.is_some());
}

#[test]
fn explicit_stop_and_error_still_clear_a_held_timeline() {
    for reason in [PlaybackEndReason::Stop, PlaybackEndReason::Error] {
        let (mut controller, _, _, events) = controller_with_mock_lifecycle(
            [near_end_status()],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = false;
        controller.play_queue_item(fixture_youtube_item("Stop retained media"), false);
        controller.update_player();
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::EndOfFileHeld);
        controller.update_player();
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::Ended(PlaybackEnd {
                reason,
                error: None,
                file_error: None,
                diagnostic: None,
            }));
        controller.update_player();
        assert!(controller.current_media.is_none());
        assert!(controller.view.playback.idle);
    }
}

#[test]
fn stale_status_cannot_unpause_or_move_a_retained_end_position() {
    for duration in [None, Some(Duration::from_secs(42))] {
        let mut active = near_end_status();
        active.duration = duration;
        let (mut controller, _, statuses, events) = controller_with_mock_lifecycle(
            [active.clone()],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = false;
        controller.play_queue_item(fixture_youtube_item("Snapshot race"), false);
        controller.update_player();
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::EndOfFileHeld);
        statuses.lock().unwrap().push_back(active);
        controller.update_player();
        assert!(controller.view.playback.paused);
        assert!(!controller.view.playback.buffering);
        assert_eq!(
            controller.view.playback.position,
            duration.unwrap_or(Duration::from_secs(40))
        );
    }
}

/// Every timeline entry point resumes only mpv's automatic end pause.
#[test]
fn seeking_back_from_held_eof_updates_the_view_to_playing_without_space() {
    for command in [
        PlayerCommand::SeekRelative(-5),
        PlayerCommand::SeekPercent(50.0),
        PlayerCommand::SeekAbsolute(Duration::from_secs(10)),
    ] {
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [near_end_status()],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = false;
        controller.play_queue_item(fixture_youtube_item("Resume from end"), false);
        controller.update_player();
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::EndOfFileHeld);
        controller.update_player();
        assert!(controller.view.playback.paused);
        state.lock().unwrap().commands.clear();

        assert!(controller.seek_player_command(command.clone()));
        if matches!(command, PlayerCommand::SeekAbsolute(_)) {
            assert!(
                controller.playback_held_at_end,
                "absolute seeks retain EOF until a native restart confirms movement"
            );
            events
                .lock()
                .unwrap()
                .push_back(PlaybackEvent::PlaybackStarted);
            controller.drain_player_events(Duration::ZERO);
        }
        assert!(
            !controller.view.playback.paused,
            "{command:?} resumes playback"
        );
        assert!(!controller.playback_held_at_end);
        assert!(controller.view.status_line.starts_with("Playing"));
        assert_eq!(state.lock().unwrap().commands, [command]);
        assert_eq!(state.lock().unwrap().played.len(), 1);
    }
}

/// Seeking a deliberate pause must not inherit automatic EOF-resume semantics.
#[test]
fn seeking_while_manually_paused_keeps_the_view_paused() {
    for command in [
        PlayerCommand::SeekRelative(-5),
        PlayerCommand::SeekPercent(50.0),
        PlayerCommand::SeekAbsolute(Duration::from_secs(10)),
    ] {
        let (mut controller, state, _, events) = controller_with_mock_lifecycle(
            [PlaybackStatus {
                paused: true,
                ..near_end_status()
            }],
            [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
        );
        controller.config.playback.autoplay = false;
        controller.play_queue_item(fixture_youtube_item("Manual pause"), false);
        controller.update_player();
        assert!(controller.view.playback.paused);
        state.lock().unwrap().commands.clear();
        assert!(controller.seek_player_command(command.clone()));
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::PlaybackStarted);
        controller.drain_player_events(Duration::ZERO);
        assert!(controller.view.playback.paused);
        assert_eq!(state.lock().unwrap().commands, [command]);
        assert_eq!(state.lock().unwrap().played.len(), 1);
    }
}

#[test]
fn rewinding_and_finishing_again_does_not_reload_or_duplicate_history() {
    let (mut controller, state, statuses, events) = controller_with_mock_lifecycle(
        [near_end_status()],
        [PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted],
    );
    controller.config.playback.autoplay = false;
    controller.play_queue_item(fixture_youtube_item("Repeated end pause"), false);
    controller.update_player();
    let history_count = controller.store.history(false, 10).unwrap().len();
    assert_eq!(history_count, 1);
    for _ in 0..2 {
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::EndOfFileHeld);
        controller.update_player();
        assert!(controller.view.playback.paused);
        assert_eq!(controller.view.playback.position, Duration::from_secs(42));
        controller.dispatch(UiAction::SeekRelative(-5));
        statuses.lock().unwrap().push_back(PlaybackStatus {
            idle: false,
            paused: false,
            position: Duration::from_secs(37),
            duration: Some(Duration::from_secs(42)),
            ..PlaybackStatus::default()
        });
        events
            .lock()
            .unwrap()
            .push_back(PlaybackEvent::PlaybackStarted);
        controller.update_player();
        assert_eq!(controller.view.playback.position, Duration::from_secs(37));
        assert!(!controller.view.playback.paused);
        assert_eq!(state.lock().unwrap().played.len(), 1);
        assert_eq!(
            controller.store.history(false, 10).unwrap().len(),
            history_count
        );
    }
}
