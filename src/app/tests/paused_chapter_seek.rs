//! Regressions for paused chapter seeks whose audio clock settles before the target.

use super::*;

/// Controller plus independently driven backend snapshots and lifecycle events.
type ChapterHarness = (
    AppController,
    Arc<Mutex<MockPlaybackState>>,
    Arc<Mutex<VecDeque<PlaybackStatus>>>,
    Arc<Mutex<VecDeque<PlaybackEvent>>>,
);

/// Starts a paused finite video using only in-memory state and mock IPC.
fn paused_controller() -> ChapterHarness {
    let (mut controller, state, statuses, events) = controller_with_mock_lifecycle([], []);
    let mut video = subscription_video_summary();
    video.duration_seconds = Some(180);
    video.description = "00:00 Intro\n01:35 Main section\n02:30 End".to_owned();
    controller.play_queue_item(queue_item_from_video(&video, None), false);
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::PlaybackStarted);
    statuses.lock().unwrap().push_back(PlaybackStatus {
        idle: false,
        paused: true,
        position: Duration::from_secs(40),
        duration: Some(Duration::from_secs(180)),
        ..PlaybackStatus::default()
    });
    controller.update_player();
    (controller, state, statuses, events)
}

/// Sends the same identity-bound action used by chapter labels and description links.
fn click(controller: &mut AppController, seconds: u64) {
    controller.dispatch(UiAction::ActivateTimecode {
        media_id: controller.current_media.clone().unwrap(),
        seconds,
    });
}

/// Reproduces the paused position observed with native mpv, without network or audio.
#[test]
fn paused_timecode_next_chapter_uses_the_requested_boundary() {
    let (mut controller, state, statuses, events) = paused_controller();
    click(&mut controller, 95);
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::PlaybackStarted);
    statuses.lock().unwrap().push_back(PlaybackStatus {
        idle: false,
        paused: true,
        position: Duration::from_micros(94_806_876),
        duration: Some(Duration::from_secs(180)),
        ..PlaybackStatus::default()
    });
    controller.update_player();
    assert!(controller.view.playback.paused);
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
    assert_eq!(
        controller.view.playback.position,
        Duration::from_micros(94_806_876),
        "the measured audio clock must not be overwritten"
    );
    state.lock().unwrap().commands.clear();
    controller.dispatch(UiAction::ChangeChapter(1));
    assert_eq!(
        state.lock().unwrap().commands,
        [PlayerCommand::SeekAbsolute(Duration::from_secs(150))],
        "the paused audio-output offset must not select the previous chapter"
    );
    controller.view.playback.position = Duration::from_millis(149_800);
    controller.dispatch(UiAction::ChangeChapter(-1));
    assert_eq!(
        state.lock().unwrap().commands.last(),
        Some(&PlayerCommand::SeekAbsolute(Duration::from_secs(95)))
    );
    controller.dispatch(UiAction::GoBack);
    assert_eq!(
        controller.view.chapter_position(),
        Duration::from_secs(150),
        "seek-back restores the logical paused cursor, not its audio-output offset"
    );
}

/// Manual pause alone must not advance the chapter before its exact boundary.
#[test]
fn ordinary_positions_near_a_chapter_are_not_rounded() {
    for paused in [false, true] {
        let (mut controller, state, _, _) = paused_controller();
        controller.view.playback.position = Duration::from_micros(94_806_876);
        controller.view.playback.paused = paused;
        assert_eq!(
            controller.view.chapter_position(),
            controller.view.playback.position
        );
        state.lock().unwrap().commands.clear();
        controller.dispatch(UiAction::ChangeChapter(1));
        assert_eq!(
            state.lock().unwrap().commands,
            [PlayerCommand::SeekAbsolute(Duration::from_secs(95))]
        );
    }
}

/// A paused marker must not leak into another seek, pause cycle, load, or end state.
#[test]
fn paused_chapter_cursor_is_cleared_at_timeline_and_lifecycle_boundaries() {
    for action in [
        "relative",
        "percent",
        "resume",
        "status-resume",
        "reload",
        "replace",
        "failure",
        "held-end",
        "reset",
    ] {
        let (mut controller, state, statuses, _) = paused_controller();
        click(&mut controller, 95);
        assert!(controller.view.paused_chapter_seek.is_some());
        match action {
            "relative" => {
                controller.seek_player_command(PlayerCommand::SeekRelative(-5));
            }
            "percent" => {
                controller.seek_player_command(PlayerCommand::SeekPercent(25.0));
            }
            "resume" => controller.player_command(PlayerCommand::TogglePause),
            "status-resume" => {
                let mut status = controller.view.playback.clone();
                status.paused = false;
                statuses.lock().unwrap().push_back(status);
                controller.update_player();
            }
            "reload" => {
                let item = controller.playback_queue.current().unwrap().clone();
                controller.play_queue_item(item, false);
            }
            "replace" => controller.play_queue_item(fixture_direct_item("another item"), false),
            "failure" => {
                state.lock().unwrap().end_before_command_error = true;
                click(&mut controller, 150);
            }
            "held-end" => {
                controller.config.playback.autoplay = false;
                controller.handle_held_playback_end(Duration::ZERO);
            }
            "reset" => controller.reset_playback_state(),
            _ => unreachable!(),
        }
        assert!(controller.view.paused_chapter_seek.is_none(), "{action}");
    }
}

/// Volume changes keep the marker, while settling before an ad's end cannot re-skip it.
#[test]
fn paused_seek_to_advertisement_end_does_not_repeat_the_seek() {
    let (mut controller, state, statuses, _) = paused_controller();
    controller.config.playback.skip_advertisement_chapters = true;
    controller.view.playback_chapters[0].title = "Реклама".to_owned();
    controller.view.playback_chapters[0].end_seconds = Some(95);
    click(&mut controller, 95);
    controller.player_command(PlayerCommand::SetVolume(25));
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
    state.lock().unwrap().commands.clear();
    let mut status = controller.view.playback.clone();
    status.position = Duration::from_micros(94_806_876);
    statuses.lock().unwrap().push_back(status);
    controller.update_player();
    assert!(state.lock().unwrap().commands.is_empty());
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
}

/// Starts an accepted video load without reporting loaded media or audible playback.
fn loading_controller() -> ChapterHarness {
    let (mut controller, state, statuses, events) = controller_with_mock_lifecycle([], []);
    let mut video = subscription_video_summary();
    video.duration_seconds = Some(180);
    video.description = "00:00 Intro\n01:35 Main section\n02:30 End".to_owned();
    controller.diagnostic_helpers_cache = Some(Vec::new());
    controller.play_queue_item(queue_item_from_video(&video, None), false);
    (controller, state, statuses, events)
}

/// Clicking while loading updates the UI immediately, not native time or durable history.
#[test]
fn loading_timestamp_highlights_immediately_and_survives_pre_start_snapshots() {
    let (mut controller, state, statuses, events) = loading_controller();
    let media_id = controller.current_media.clone().unwrap();
    click(&mut controller, 95);
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
    assert_eq!(controller.view.chapter_media_id(), Some(&media_id));
    assert!(controller.view.playing_media_id.is_none());
    assert!(controller.view.playback_starting);
    assert_eq!(controller.view.playback.position, Duration::ZERO);
    assert_eq!(
        controller
            .pending_history
            .as_ref()
            .unwrap()
            .position_seconds,
        0
    );
    assert_eq!(
        state.lock().unwrap().commands.last(),
        Some(&PlayerCommand::SeekAbsolute(Duration::from_secs(95)))
    );
    for idle in [true, false] {
        statuses.lock().unwrap().push_back(PlaybackStatus {
            idle,
            paused: false,
            duration: Some(Duration::from_secs(180)),
            ..PlaybackStatus::default()
        });
        controller.update_player();
        assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
        assert!(controller.view.playing_media_id.is_none());
    }
    events.lock().unwrap().push_back(PlaybackEvent::MediaLoaded);
    controller.update_player();
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
    click(&mut controller, 150);
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(150));
    controller.dispatch(UiAction::GoBack);
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
    controller.player_command(PlayerCommand::TogglePause);
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
}

/// Pending visual feedback follows the load lifecycle instead of leaking to other files.
#[test]
fn loading_timestamp_is_retired_on_cancel_failure_replacement_or_playback_start() {
    for action in [
        "stop",
        "replace",
        "reload",
        "failure",
        "relative",
        "started",
        "started-paused",
    ] {
        let (mut controller, state, statuses, events) = loading_controller();
        click(&mut controller, 95);
        assert!(controller.view.loading_chapter_seek.is_some());
        match action {
            "stop" => controller.player_command(PlayerCommand::Stop),
            "replace" => controller.play_queue_item(fixture_direct_item("replacement"), false),
            "reload" => {
                let item = controller.playback_queue.current().unwrap().clone();
                controller.play_queue_item(item, false);
            }
            "failure" => {
                state.lock().unwrap().end_before_command_error = true;
                click(&mut controller, 150);
            }
            "relative" => {
                controller.seek_player_command(PlayerCommand::SeekRelative(5));
            }
            "started" | "started-paused" => {
                events
                    .lock()
                    .unwrap()
                    .extend([PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted]);
                statuses.lock().unwrap().push_back(PlaybackStatus {
                    idle: false,
                    paused: action == "started-paused",
                    position: Duration::from_micros(94_806_876),
                    duration: Some(Duration::from_secs(180)),
                    ..PlaybackStatus::default()
                });
                controller.update_player();
                let expected = if action == "started-paused" {
                    Duration::from_secs(95)
                } else {
                    Duration::from_micros(94_806_876)
                };
                assert_eq!(controller.view.chapter_position(), expected);
                assert!(!controller.view.playback_starting);
            }
            _ => unreachable!(),
        }
        assert!(controller.view.loading_chapter_seek.is_none(), "{action}");
    }
}

/// A transparent same-item retry retains the requested timestamp and its feedback.
#[test]
fn loading_timestamp_survives_youtube_retry_and_ignored_replacement_stop() {
    let (mut controller, state, _, events) = loading_controller();
    click(&mut controller, 95);
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::Ended(PlaybackEnd {
            reason: PlaybackEndReason::Error,
            error: Some("loading failed".to_owned()),
            file_error: Some("unrecognized file format".to_owned()),
            diagnostic: None,
        }));
    controller.update_player();
    assert_eq!(state.lock().unwrap().played.len(), 2);
    assert_eq!(
        state.lock().unwrap().played[1].start_at,
        Duration::from_secs(95)
    );
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::Ended(PlaybackEnd {
            reason: PlaybackEndReason::Stop,
            error: None,
            file_error: None,
            diagnostic: None,
        }));
    controller.update_player();
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
    assert!(controller.view.playback_starting);
    assert!(controller.view.playing_media_id.is_none());
}

/// An old idle snapshot cannot erase the paused cursor transferred at startup.
#[test]
fn loading_timestamp_paused_handoff_ignores_placeholder_status() {
    let (mut controller, _, statuses, events) = loading_controller();
    click(&mut controller, 95);
    events
        .lock()
        .unwrap()
        .push_back(PlaybackEvent::PlaybackStarted);
    controller.update_player(); // The mock still returns its default idle snapshot.
    statuses.lock().unwrap().push_back(PlaybackStatus {
        idle: false,
        paused: true,
        position: Duration::from_micros(94_806_876),
        duration: Some(Duration::from_secs(180)),
        ..PlaybackStatus::default()
    });
    controller.update_player();
    assert_eq!(controller.view.chapter_position(), Duration::from_secs(95));
    assert_eq!(
        controller.view.playback.position,
        Duration::from_micros(94_806_876)
    );
}
