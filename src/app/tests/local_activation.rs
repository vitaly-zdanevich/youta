//! Enter pauses the active Local file without changing ordinary row activation.

use super::*;

/// Sends a real unmodified key through the shared frontend keymap.
fn press(controller: &mut AppController, key: crate::keymap::Key) {
    let action = crate::keymap::key_action(
        crate::keymap::KeyPress::new(key),
        &controller.view,
        None,
        None,
    )
    .expect("mapped key");
    controller.dispatch(action);
}

/// Selects an exact Local path through an accepted directory snapshot.
fn select_path(controller: &mut AppController, path: &Path) {
    controller.view.screen = Screen::Local;
    controller.handle_local_directory_response(
        controller.local_generation,
        crate::local_browser::list_local_directory(
            path.parent().unwrap(),
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .map_err(|error| error.to_string()),
    );
    controller.select_local_path(Some(path));
    controller.refresh_local_browser_rows();
}

/// Starts a fake local file through Enter with no real player or metadata probe.
fn playing_fixture(
    extension: &str,
) -> (
    tempfile::TempDir,
    AppController,
    Arc<Mutex<MockPlaybackState>>,
    PathBuf,
) {
    let temporary = crate::test_support::canonical_tempdir("Local Enter 音楽");
    let path = temporary.path().join(format!("track.{extension}"));
    std::fs::write(&path, b"mock media").unwrap();
    let mut config = Config::for_dir(temporary.path().join("config"));
    config.ui.show_local_folder_sizes = false;
    config.providers.ffprobe_executable = temporary.path().join("no-ffprobe");
    let (factory, state, _, _) = mock_playback_factory([], []);
    let mut controller = AppController::new(
        config,
        StateStore::open_in_memory().unwrap(),
        None,
        Some(factory),
    );
    select_path(&mut controller, &path);
    press(&mut controller, crate::keymap::Key::Enter);
    assert_eq!(state.lock().unwrap().played.len(), 1);
    state.lock().unwrap().commands.clear();
    (temporary, controller, state, path)
}

/// Audio, video and tracker files use the same toggle as Space, without another load.
#[test]
fn local_enter_toggles_active_file_without_reloading() {
    for extension in ["opus", "mp4", "mod"] {
        for paused in [false, true] {
            let (_temporary, mut controller, state, _) = playing_fixture(extension);
            controller.playback_phase = PlaybackPhase::Playing;
            controller.view.playback.paused = paused;
            controller.view.playback.position = Duration::from_secs(42);
            let origin = controller.current_autoplay_origin.clone();
            press(&mut controller, crate::keymap::Key::Enter);
            assert_eq!(state.lock().unwrap().commands, [PlayerCommand::TogglePause]);
            assert_eq!(state.lock().unwrap().played.len(), 1);
            assert_eq!(controller.view.playback.position, Duration::from_secs(42));
            assert_eq!(controller.current_autoplay_origin, origin);
            state.lock().unwrap().commands.clear();
            press(&mut controller, crate::keymap::Key::Char(' '));
            assert_eq!(state.lock().unwrap().commands, [PlayerCommand::TogglePause]);
        }
    }
}

/// An accepted load must not restart while the backend is still starting it.
#[test]
fn local_enter_toggles_loading_file_without_reloading() {
    for phase in [PlaybackPhase::Loading, PlaybackPhase::Loaded] {
        let (_temporary, mut controller, state, _) = playing_fixture("opus");
        controller.playback_phase = phase;
        press(&mut controller, crate::keymap::Key::Enter);
        assert_eq!(state.lock().unwrap().commands, [PlayerCommand::TogglePause]);
        assert_eq!(state.lock().unwrap().played.len(), 1);
    }
}

/// Matching labels never turn activation of a different path into pause.
#[test]
fn local_enter_starts_other_file_with_same_basename() {
    let (temporary, mut controller, state, first) = playing_fixture("opus");
    controller.playback_phase = PlaybackPhase::Playing;
    let second = temporary.path().join("other/track.opus");
    std::fs::create_dir(second.parent().unwrap()).unwrap();
    std::fs::write(&second, b"other mock media").unwrap();
    select_path(&mut controller, &second);
    // Details are not authoritative for the selected row.
    controller.view.details.as_mut().unwrap().media_id = Some(local_media_id(&first));
    press(&mut controller, crate::keymap::Key::Enter);
    assert_eq!(controller.current_media, Some(local_media_id(&second)));
    assert_eq!(state.lock().unwrap().played.len(), 2);
    assert!(
        !state
            .lock()
            .unwrap()
            .commands
            .contains(&PlayerCommand::TogglePause)
    );
}

/// Legacy Local path IDs match, but an unrelated provider's locator does not.
#[test]
fn local_enter_matches_legacy_local_identity_only() {
    for source in [SourceKind::Local, SourceKind::RemoteFiles] {
        let (_temporary, mut controller, state, path) = playing_fixture("opus");
        controller.playback_phase = PlaybackPhase::Playing;
        controller.current_media = Some(MediaId::new(source.clone(), path.to_str().unwrap()));
        press(&mut controller, crate::keymap::Key::Enter);
        if source == SourceKind::Local {
            assert_eq!(state.lock().unwrap().played.len(), 1);
            assert_eq!(state.lock().unwrap().commands, [PlayerCommand::TogglePause]);
        } else {
            assert_eq!(state.lock().unwrap().played.len(), 2);
            assert!(
                !state
                    .lock()
                    .unwrap()
                    .commands
                    .contains(&PlayerCommand::TogglePause)
            );
        }
    }
}

/// An idle retained identity or restored selection still starts playback normally.
#[test]
fn local_enter_starts_idle_or_restored_file() {
    for backend_present in [false, true] {
        let (_temporary, mut controller, state, _) = playing_fixture("opus");
        controller.playback_phase = PlaybackPhase::Idle;
        if !backend_present {
            controller.player = None;
        }
        press(&mut controller, crate::keymap::Key::Enter);
        assert_eq!(state.lock().unwrap().played.len(), 2);
        assert!(
            !state
                .lock()
                .unwrap()
                .commands
                .contains(&PlayerCommand::TogglePause)
        );
    }
}

/// Folder and parent activation keep navigating while a Local file is active.
#[test]
fn local_enter_opens_folders_without_pausing_playback() {
    for parent in [false, true] {
        let (temporary, mut controller, state, _) = playing_fixture("opus");
        controller.playback_phase = PlaybackPhase::Playing;
        let folder = temporary.path().join("album");
        std::fs::create_dir(&folder).unwrap();
        select_path(&mut controller, &folder);
        if parent {
            controller.view.selected = 0;
        }
        let (requests, received) = unbounded();
        controller.local_browse_requests = Some(requests);
        press(&mut controller, crate::keymap::Key::Enter);
        assert!(matches!(
            received.try_recv(),
            Ok(LocalBrowseRequest::Browse { .. })
        ));
        assert_eq!(state.lock().unwrap().played.len(), 1);
        assert!(state.lock().unwrap().commands.is_empty());
    }
}
