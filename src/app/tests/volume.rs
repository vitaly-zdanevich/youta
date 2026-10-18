//! Remember the user's volume without depending on playback history or shutdown.

use super::*;

/// Uses an isolated configuration and a backend that cannot touch real audio.
fn volume_controller() -> (
    tempfile::TempDir,
    AppController,
    Arc<Mutex<MockPlaybackState>>,
) {
    let directory = tempfile::tempdir().unwrap();
    let config = Config::for_dir(directory.path());
    let (factory, state, _, _) = mock_playback_factory([], []);
    let controller = AppController::new(
        config,
        StateStore::open_in_memory().unwrap(),
        None,
        Some(factory),
    );
    (directory, controller, state)
}

/// Reopening Youta must restore an accepted change even without playing first.
#[test]
fn volume_survives_restart_before_playback_with_history_disabled() {
    let (directory, mut controller, _) = volume_controller();
    controller.config.persistence.save_playback_history = false;
    controller.dispatch(UiAction::ChangeVolume(-35));
    assert_eq!(controller.view.playback.volume, 45);
    assert!(controller.player.is_none());
    drop(controller);
    let config = Config::load_from_dir(directory.path()).unwrap();
    let restored = AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    assert_eq!(restored.view.playback.volume, 45);
}

/// Rapid keypresses accumulate before the asynchronous player's next snapshot.
#[test]
fn volume_changes_while_playing_are_saved_and_survive_reset() {
    let (directory, mut controller, state) = volume_controller();
    controller.play_queue_item(fixture_direct_item("volume fixture"), false);
    state.lock().unwrap().commands.clear();
    controller.dispatch(UiAction::ChangeVolume(-5));
    controller.dispatch(UiAction::ChangeVolume(-5));
    assert_eq!(
        state.lock().unwrap().commands,
        [PlayerCommand::SetVolume(75), PlayerCommand::SetVolume(70)]
    );
    assert_eq!(
        Config::load_from_dir(directory.path())
            .unwrap()
            .playback
            .volume_percent,
        70
    );
    controller.reset_playback_state();
    assert_eq!(controller.view.playback.volume, 70);
    drop(controller);
    let (factory, restarted, _, _) = mock_playback_factory([], []);
    let mut restored = AppController::new(
        Config::load_from_dir(directory.path()).unwrap(),
        StateStore::open_in_memory().unwrap(),
        None,
        Some(factory),
    );
    restored.play_queue_item(fixture_direct_item("restarted volume fixture"), false);
    assert_eq!(
        restarted.lock().unwrap().commands.first(),
        Some(&PlayerCommand::SetVolume(70))
    );
}

/// Direct output must not create a software-volume preference it cannot apply.
#[test]
fn volume_in_direct_profile_is_not_changed_or_saved() {
    let (directory, mut controller, _) = volume_controller();
    controller.config.playback.audiophile.enabled = true;
    controller.dispatch(UiAction::ChangeVolume(-5));
    assert_eq!(controller.view.playback.volume, 80);
    assert_eq!(controller.config.playback.volume_percent, 80);
    assert!(!directory.path().join("config.toml").exists());
    assert!(controller.view.status_line.contains("software volume"));
}

/// A failed save must neither erase the config nor undo the audible adjustment.
#[test]
fn volume_save_failure_keeps_runtime_value_and_reports_failure() {
    let (directory, mut controller, _) = volume_controller();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[malformed").unwrap();
    controller.dispatch(UiAction::ChangeVolume(-5));
    assert_eq!(controller.view.playback.volume, 75);
    assert_eq!(controller.config.playback.volume_percent, 75);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "[malformed");
    assert!(controller.view.status_line.contains("could not remember"));
}

/// Cached status cannot undo a new value, but later external volume changes work.
#[test]
fn volume_masks_stale_snapshots_until_acknowledged() {
    let (_directory, mut controller, state) = volume_controller();
    controller.play_queue_item(fixture_direct_item("volume fixture"), false);
    controller.dispatch(UiAction::ChangeVolume(-5));
    // Keep acknowledgement testing independent of config fsync latency; expiry
    // has its own deterministic regression below.
    let (volume, deadline) = controller.pending_volume.as_mut().expect("pending volume");
    assert_eq!(*volume, 75);
    *deadline = Instant::now() + Duration::from_secs(3600);
    let mut stale = PlaybackStatus {
        volume: 80,
        ..PlaybackStatus::default()
    };
    controller.reconcile_pending_volume(&mut stale);
    assert_eq!(stale.volume, 75);
    controller.view.playback = stale;
    controller.dispatch(UiAction::ChangeVolume(-5));
    assert_eq!(
        state.lock().unwrap().commands.last(),
        Some(&PlayerCommand::SetVolume(70))
    );
    let mut fresh = PlaybackStatus {
        volume: 70,
        ..PlaybackStatus::default()
    };
    controller.reconcile_pending_volume(&mut fresh);
    assert!(controller.pending_volume.is_none());
    let mut external = PlaybackStatus {
        volume: 25,
        ..PlaybackStatus::default()
    };
    controller.reconcile_pending_volume(&mut external);
    controller.view.playback = external;
    controller.dispatch(UiAction::ChangeVolume(5));
    assert_eq!(
        state.lock().unwrap().commands.last(),
        Some(&PlayerCommand::SetVolume(30))
    );
}

/// Persist silence and the maximum without allowing values outside 0..=100.
#[test]
fn volume_clamps_and_persists_both_endpoints() {
    let (directory, mut controller, _) = volume_controller();
    for (delta, expected) in [(-127, 0), (127, 100)] {
        controller.dispatch(UiAction::ChangeVolume(delta));
        assert_eq!(controller.view.playback.volume, expected);
        assert_eq!(
            Config::load_from_dir(directory.path())
                .unwrap()
                .playback
                .volume_percent,
            expected
        );
    }
}

/// Coalesced external changes cannot leave the temporary optimistic value stuck.
#[test]
fn volume_superseded_before_acknowledgement_expires_without_sleeping() {
    let (_directory, mut controller, _) = volume_controller();
    controller.pending_volume = Some((75, Instant::now() - Duration::from_secs(1)));
    let mut external = PlaybackStatus {
        volume: 25,
        ..PlaybackStatus::default()
    };
    controller.reconcile_pending_volume(&mut external);
    assert!(controller.pending_volume.is_none());
    assert_eq!(external.volume, 25);
}

/// Playback rejection must not become the volume used on the next launch.
#[test]
fn volume_rejected_by_backend_is_not_saved() {
    let (directory, mut controller, state) = volume_controller();
    controller.play_queue_item(fixture_direct_item("volume fixture"), false);
    state.lock().unwrap().end_before_command_error = true;
    controller.dispatch(UiAction::ChangeVolume(-5));
    assert_eq!(controller.view.playback.volume, 80);
    assert_eq!(controller.config.playback.volume_percent, 80);
    assert!(controller.pending_volume.is_none());
    assert!(!directory.path().join("config.toml").exists());
}

/// Environment overrides retain startup precedence without blocking runtime control.
#[test]
fn volume_environment_override_only_changes_current_session() {
    const CHILD: &str = "YOUTA_VOLUME_CONTROLLER_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::load_from_dir(directory.path()).unwrap();
        assert_eq!(config.playback.volume_percent, 35);
        let mut controller =
            AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        controller.dispatch(UiAction::ChangeVolume(5));
        assert_eq!(controller.view.playback.volume, 40);
        assert_eq!(controller.config.playback.volume_percent, 40);
        assert!(!directory.path().join("config.toml").exists());
        assert_eq!(
            Config::load_from_dir(directory.path())
                .unwrap()
                .playback
                .volume_percent,
            35
        );
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app::tests::volume_tests::volume_environment_override_only_changes_current_session",
        ])
        .env(CHILD, "1")
        .env(crate::config::VOLUME_PERCENT_ENV, "35")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
