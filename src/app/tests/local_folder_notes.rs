//! Local directories use private source notes without acquiring playback identities.

use super::*;

/// Creates real folders with identical basenames under separate parents.
fn folder_fixture() -> (tempfile::TempDir, Config, PathBuf, PathBuf) {
    let temporary = crate::test_support::canonical_tempdir("folder notes");
    let mut config = Config::for_dir(temporary.path().join("config"));
    config.ui.show_local_folder_sizes = false;
    let first = temporary.path().join("first/album");
    let second = temporary.path().join("second/album");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    (temporary, config, first, second)
}

/// Selects an exact child folder through the normal listing and Details projections.
fn select_folder(controller: &mut AppController, path: &Path) {
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
    controller.refresh_selected_playlist_state();
}

/// A selected folder can be annotated without becoming playable or playlist eligible.
#[test]
fn local_folder_note_is_a_nonplayable_source_with_exact_path_identity() {
    let (_temporary, config, first, second) = folder_fixture();
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    select_folder(&mut controller, &first);
    assert!(controller.view.private_note_available);
    assert!(controller.view.details.as_ref().unwrap().media_id.is_none());
    assert!(
        controller.view.rows[controller.view.selected]
            .media_id
            .is_none()
    );
    assert!(controller.selected_playlist_snapshot().is_err());
    let target = CommentTarget::Source {
        source_id: local_media_id(&first),
    };
    assert_eq!(
        controller.selected_private_note_target().unwrap().target,
        target
    );
    controller.config.ui.show_full_local_paths = true;
    controller.update_local_browser_detail();
    assert_eq!(
        controller.selected_private_note_target().unwrap().target,
        target
    );
    select_folder(&mut controller, &second);
    assert_ne!(
        controller.selected_private_note_target().unwrap().target,
        target
    );
}

/// Folder notes support save/edit/cancel/delete and survive reopening the file backend.
#[test]
fn local_folder_note_lifecycle_restart_and_child_independence() {
    let (_temporary, config, first, second) = folder_fixture();
    let target = CommentTarget::Source {
        source_id: local_media_id(&first),
    };
    let child = CommentTarget::Media {
        media_id: local_media_id(&first.join("track.mp3")),
    };
    let same_path_media = CommentTarget::Media {
        media_id: local_media_id(&first),
    };
    {
        let store = StateStore::open(&config).unwrap();
        store.upsert_private_note(&child, "child note", 1).unwrap();
        store
            .upsert_private_note(&same_path_media, "media note", 1)
            .unwrap();
        let mut controller = AppController::new(config.clone(), store, None, None);
        select_folder(&mut controller, &first);
        controller.dispatch(UiAction::EditPrivateNote);
        assert!(
            !controller
                .view
                .private_note_popup
                .as_ref()
                .unwrap()
                .existing
        );
        assert!(
            controller
                .view
                .private_note_popup
                .as_ref()
                .unwrap()
                .target_label
                .contains("album")
        );
        for character in "Album notes\nsecond line".chars() {
            controller.dispatch(UiAction::AppendPrivateNoteCharacter(character));
        }
        controller.dispatch(UiAction::SavePrivateNote);
        assert!(controller.view.details.as_ref().unwrap().has_private_note);
        select_folder(&mut controller, &second);
        assert!(!controller.view.details.as_ref().unwrap().has_private_note);
        controller.shutdown_for_exit().unwrap();
    }
    let store = StateStore::open(&config).unwrap();
    let mut controller = AppController::new(config, store, None, None);
    select_folder(&mut controller, &first);
    controller.dispatch(UiAction::EditPrivateNote);
    assert_eq!(
        controller.view.private_note_popup.as_ref().unwrap().body,
        "Album notes\nsecond line"
    );
    assert!(
        controller
            .view
            .private_note_popup
            .as_ref()
            .unwrap()
            .existing
    );
    controller.dispatch(UiAction::AppendPrivateNoteCharacter('!'));
    controller.dispatch(UiAction::DismissPrivateNotePopup);
    controller.dispatch(UiAction::EditPrivateNote);
    assert_eq!(
        controller.view.private_note_popup.as_ref().unwrap().body,
        "Album notes\nsecond line"
    );
    controller.dispatch(UiAction::AppendPrivateNoteCharacter('!'));
    controller.dispatch(UiAction::SavePrivateNote);
    assert_eq!(
        controller
            .store
            .private_note(&target)
            .unwrap()
            .unwrap()
            .body,
        "Album notes\nsecond line!"
    );
    controller.dispatch(UiAction::EditPrivateNote);
    controller.dispatch(UiAction::RequestPrivateNoteDelete);
    assert!(controller.store.private_note(&target).unwrap().is_some());
    controller.dispatch(UiAction::RequestPrivateNoteDelete);
    assert!(controller.store.private_note(&target).unwrap().is_none());
    assert!(!controller.view.details.as_ref().unwrap().has_private_note);
    assert_eq!(
        controller.store.private_note(&child).unwrap().unwrap().body,
        "child note"
    );
    assert_eq!(
        controller
            .store
            .private_note(&same_path_media)
            .unwrap()
            .unwrap()
            .body,
        "media note"
    );
}

/// The parent row reuses the same directory identity as its ordinary child row.
#[test]
fn local_folder_note_parent_row_refers_to_the_displayed_parent() {
    let (_temporary, config, first, _) = folder_fixture();
    let nested = first.join("disc");
    std::fs::create_dir(&nested).unwrap();
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    select_folder(&mut controller, &first);
    let expected = controller.selected_private_note_target().unwrap().target;
    controller.local_listing = Some(
        crate::local_browser::list_local_directory(
            &nested,
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .unwrap(),
    );
    controller.view.selected = 0;
    controller.refresh_local_browser_rows();
    controller.refresh_selected_playlist_state();
    assert!(controller.view.private_note_available);
    assert!(controller.view.details.as_ref().unwrap().media_id.is_none());
    assert_eq!(
        controller.selected_private_note_target().unwrap().target,
        expected
    );
    assert!(
        controller
            .selected_private_note_target()
            .unwrap()
            .label
            .contains("album")
    );
}

/// Local row ownership overrides stale channel Details and rejects pending rows.
#[test]
fn local_folder_note_ignores_stale_details_and_loading_rows() {
    let (_temporary, config, first, _) = folder_fixture();
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    select_folder(&mut controller, &first);
    controller.view.right_panel_mode = RightPanelMode::Channel;
    controller.view.details = Some(DetailView {
        channel_id: "stale-channel".to_owned(),
        media_id: Some(MediaId::new(SourceKind::YouTube, "stale-video")),
        ..DetailView::default()
    });
    assert_eq!(
        controller.selected_private_note_target().unwrap().target,
        CommentTarget::Source {
            source_id: local_media_id(&first)
        }
    );
    controller.view.selected = usize::MAX;
    assert!(controller.selected_private_note_target().is_none());
    select_folder(&mut controller, &first);
    controller.view.local_browse_pending = true;
    assert!(controller.selected_private_note_target().is_none());
    controller.view.local_browse_pending = false;
    controller.browse_local_directory(first);
    assert!(!controller.view.private_note_available);
    assert!(controller.selected_private_note_target().is_none());
    controller.handle_local_directory_response(
        controller.local_generation,
        Err("fixture error".to_owned()),
    );
    assert!(!controller.view.private_note_available);
    assert!(controller.selected_private_note_target().is_none());
}

/// Rebuilding folder Details must retain the saved-note indicator.
#[test]
fn local_folder_note_indicator_survives_sort_and_path_display_changes() {
    let (_temporary, config, first, _) = folder_fixture();
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    let target = CommentTarget::Source {
        source_id: local_media_id(&first),
    };
    controller
        .store
        .upsert_private_note(&target, "folder note", 1)
        .unwrap();
    select_folder(&mut controller, &first);
    controller.config.ui.show_local_folder_sizes = true;
    controller.toggle_local_size_sort();
    assert!(controller.view.private_note_available);
    assert!(controller.view.details.as_ref().unwrap().has_private_note);
    controller.config.ui.show_full_local_paths = true;
    controller.update_local_browser_detail();
    assert!(controller.view.details.as_ref().unwrap().has_private_note);
}

/// At an archive root, `..` annotates the visible source parent, not the cache.
#[cfg(feature = "local-archives")]
#[test]
fn local_folder_note_archive_parent_uses_logical_directory() {
    let (temporary, config, first, _) = folder_fixture();
    let root = temporary.path().join("archive-cache/contents");
    std::fs::create_dir_all(&root).unwrap();
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    controller.view.screen = Screen::Local;
    controller
        .local_archive_stack
        .push(crate::local_archive::MaterializedLocalArchive {
            source_path: first.join("album.zip"),
            root_path: root.clone(),
            reused_cache: false,
        });
    controller.local_listing = Some(
        crate::local_browser::list_local_directory(
            &root,
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .unwrap(),
    );
    controller.view.selected = 0;
    controller.refresh_local_browser_rows();
    assert_eq!(
        controller.selected_private_note_target().unwrap().target,
        CommentTarget::Source {
            source_id: local_media_id(&first)
        }
    );
    assert_eq!(
        controller.selected_private_note_target().unwrap().label,
        controller.local_display_path(&first)
    );
}

/// Lossy display labels cannot merge distinct Unix directory identities.
#[cfg(unix)]
#[test]
fn local_folder_note_preserves_non_utf8_directory_identity() {
    use std::os::unix::ffi::OsStringExt;

    let (temporary, config, _, _) = folder_fixture();
    let first = temporary
        .path()
        .join(std::ffi::OsString::from_vec(b"album\xfe".to_vec()));
    let second = temporary
        .path()
        .join(std::ffi::OsString::from_vec(b"album\xff".to_vec()));
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    select_folder(&mut controller, &first);
    let target = controller.selected_private_note_target().unwrap().target;
    select_folder(&mut controller, &second);
    assert_ne!(
        controller.selected_private_note_target().unwrap().target,
        target
    );
}
