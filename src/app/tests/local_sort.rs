//! Saved Local ordering keeps draft edits isolated and selected paths stable.

use super::*;

/// Builds a real bounded listing and isolated configuration without background scans.
fn local_sort_fixture() -> (tempfile::TempDir, AppController) {
    let fixture = crate::test_support::canonical_tempdir("Local natural-sort fixture");
    let media = fixture.path().join("media");
    std::fs::create_dir(&media).expect("media folder");
    for name in ["10.mp3", "2.mp3", "1.mp3"] {
        std::fs::write(media.join(name), b"xx").expect("track fixture");
    }
    for name in ["folder10", "folder2"] {
        std::fs::create_dir(media.join(name)).expect("folder fixture");
    }
    let mut config = Config::for_dir(fixture.path().join("config"));
    config.ui.show_local_folder_sizes = false;
    let store = StateStore::open_in_memory().expect("in-memory state");
    let mut controller = AppController::new(config, store, None, None);
    controller.view.screen = Screen::Local;
    controller.local_listing = Some(
        crate::local_browser::list_local_directory(
            &media,
            crate::local_browser::LocalBrowseLimits::default(),
        )
        .expect("fixture listing"),
    );
    controller.refresh_local_browser_rows();
    (fixture, controller)
}

/// Returns exact fixture names, independently of presentation labels.
fn names(controller: &AppController) -> Vec<String> {
    controller
        .local_listing
        .as_ref()
        .expect("listing")
        .entries
        .iter()
        .map(|entry| entry.name.to_str().expect("fixture name").to_owned())
        .collect()
}

#[test]
fn natural_local_sort_draft_cancel_and_save_preserve_selection_without_rescan() {
    let (_fixture, mut controller) = local_sort_fixture();
    let selected = controller
        .local_listing
        .as_ref()
        .unwrap()
        .path
        .join("2.mp3");
    controller.select_local_path(Some(&selected));
    let item = queue_item_from_local(&local_media_item_stub(selected.clone(), Some(2)))
        .expect("queue item");
    controller.current_media = Some(item.media.id.clone());
    controller.playback_queue.push(item);
    controller.current_autoplay_origin = Some(AutoplayOrigin::LocalBrowser {
        directory: selected.parent().unwrap().to_owned(),
        entries: Arc::from([selected.clone(), selected.with_file_name("10.mp3")]),
        index: 0,
    });
    let origin = controller.current_autoplay_origin.clone();
    let queue = controller.playback_queue.clone();
    let media_id = controller.current_media.clone();
    let lexical = ["folder10", "folder2", "1.mp3", "10.mp3", "2.mp3"];
    assert_eq!(names(&controller), lexical);
    controller.dispatch(UiAction::OpenPreferences);
    controller.dispatch(UiAction::ToggleNaturalLocalSort);
    assert!(
        controller
            .view
            .preferences_popup
            .as_ref()
            .unwrap()
            .natural_local_sort
    );
    assert!(!controller.config.ui.natural_local_sort);
    assert_eq!(names(&controller), lexical);
    controller.dispatch(UiAction::DismissPreferences);
    assert!(!controller.config.config_file().exists());
    controller.dispatch(UiAction::OpenPreferences);
    assert!(
        !controller
            .view
            .preferences_popup
            .as_ref()
            .unwrap()
            .natural_local_sort
    );
    controller.dispatch(UiAction::ToggleNaturalLocalSort);
    let generation = controller.local_generation;
    let size_generation = controller
        .local_folder_size_generation
        .load(AtomicOrdering::Relaxed);
    controller.dispatch(UiAction::SubmitPreferences);
    assert!(controller.view.preferences_popup.is_none());
    assert!(controller.config.ui.natural_local_sort);
    assert_eq!(
        names(&controller),
        ["folder2", "folder10", "1.mp3", "2.mp3", "10.mp3"]
    );
    assert_eq!(
        controller.selected_local_path().as_deref(),
        Some(selected.as_path())
    );
    assert_eq!(controller.local_generation, generation);
    assert_eq!(
        controller
            .local_folder_size_generation
            .load(AtomicOrdering::Relaxed),
        size_generation
    );
    assert!(!controller.view.local_browse_pending);
    assert!(controller.local_folder_size_pending.is_none());
    assert_eq!(controller.current_autoplay_origin, origin);
    assert_eq!(controller.playback_queue, queue);
    assert_eq!(controller.current_media, media_id);
    assert!(
        Config::load_from_dir(controller.config.config_dir())
            .unwrap()
            .ui
            .natural_local_sort
    );
    controller.dispatch(UiAction::OpenPreferences);
    controller.dispatch(UiAction::ToggleNaturalLocalSort);
    controller.dispatch(UiAction::SubmitPreferences);
    assert_eq!(names(&controller), lexical);
    assert_eq!(
        controller.selected_local_path().as_deref(),
        Some(selected.as_path())
    );
}

#[test]
fn natural_local_sort_only_breaks_size_ties_and_keeps_unknown_folders_last() {
    let (_fixture, mut controller) = local_sort_fixture();
    controller.config.ui.natural_local_sort = true;
    controller.config.ui.show_local_folder_sizes = true;
    let listing = controller.local_listing.as_mut().unwrap();
    listing
        .entries
        .iter_mut()
        .find(|entry| entry.name == "1.mp3")
        .unwrap()
        .size_bytes = Some(9);
    controller.view.local_size_sort = LocalSizeSort::Ascending;
    controller.sort_local_listing();
    assert_eq!(
        names(&controller),
        ["2.mp3", "10.mp3", "1.mp3", "folder2", "folder10"]
    );
    controller.view.local_size_sort = LocalSizeSort::Descending;
    controller.sort_local_listing();
    assert_eq!(
        names(&controller),
        ["1.mp3", "2.mp3", "10.mp3", "folder2", "folder10"]
    );
}

#[test]
fn natural_local_sort_applies_current_preference_to_older_worker_listing() {
    let (_fixture, mut controller) = local_sort_fixture();
    let listing = controller.local_listing.take().unwrap();
    controller.config.ui.natural_local_sort = true;
    controller.handle_local_directory_response(controller.local_generation, Ok(listing));
    assert_eq!(
        names(&controller),
        ["folder2", "folder10", "1.mp3", "2.mp3", "10.mp3"]
    );
}

#[test]
fn natural_local_sort_environment_lock_does_not_mutate_draft_or_disk() {
    let (_fixture, mut controller) = local_sort_fixture();
    controller.open_preferences();
    controller
        .view
        .preferences_popup
        .as_mut()
        .unwrap()
        .environment_override = Some(NATURAL_LOCAL_SORT_ENV.to_owned());
    controller.dispatch(UiAction::ToggleNaturalLocalSort);
    assert!(
        !controller
            .view
            .preferences_popup
            .as_ref()
            .unwrap()
            .natural_local_sort
    );
    assert!(
        controller
            .view
            .preferences_popup
            .as_ref()
            .unwrap()
            .validation_error
            .is_some()
    );
    controller.dispatch(UiAction::SubmitPreferences);
    assert!(!controller.config.ui.natural_local_sort);
    assert!(!controller.config.config_file().exists());
}

#[test]
fn natural_local_sort_save_from_another_screen_preserves_its_selection_and_rows() {
    for also_change_sizes in [false, true] {
        let (_fixture, mut controller) = local_sort_fixture();
        controller.view.screen = Screen::Search;
        controller.view.selected = 2;
        controller.view.rows = vec![RowView::default(); 4];
        let rows = controller.view.rows.clone();
        let details = controller.view.details.clone();
        controller.dispatch(UiAction::OpenPreferences);
        controller.dispatch(UiAction::ToggleNaturalLocalSort);
        if also_change_sizes {
            controller.dispatch(UiAction::ToggleLocalFolderSizes);
        }
        controller.dispatch(UiAction::SubmitPreferences);
        assert_eq!(controller.view.selected, 2);
        assert_eq!(controller.view.rows, rows);
        assert_eq!(controller.view.details, details);
        assert_eq!(
            names(&controller),
            ["folder2", "folder10", "1.mp3", "2.mp3", "10.mp3"]
        );
    }
}
