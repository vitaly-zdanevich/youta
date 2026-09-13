//! Deterministic controller tests: no Internet Archive or YouTube access.

use super::*;

/// A controller with an isolated store and no remote provider worker.
fn controller() -> (tempfile::TempDir, AppController) {
    let directory = crate::test_support::canonical_tempdir("archive controller");
    let config = Config::for_dir(directory.path().join("config"));
    let store = StateStore::open_in_memory().unwrap();
    let mut controller = AppController::new(config, store, None, None);
    controller.view.screen = Screen::ArchiveOrg;
    controller.archive_org.initialized = true;
    (directory, controller)
}

fn details(identifier: &str) -> Arc<ArchiveOrgItemDetails> {
    Arc::new(
        serde_json::from_value(serde_json::json!({
            "item": {
                "identifier": identifier, "title": "An audio collection",
                "webpage_url": format!("https://archive.org/details/{identifier}"),
                "collections": [], "topics": [], "languages": []
            },
            "tracks": [{"filename": "01.opus", "title": "First",
                "download_url": format!("https://archive.org/download/{identifier}/01.opus")},
                {"filename": "02.opus", "title": "Second",
                "download_url": format!("https://archive.org/download/{identifier}/02.opus")}],
            "comments": []
        }))
        .unwrap(),
    )
}

/// Captures a pending owner without starting a thread or waiting on a clock.
fn pending(
    controller: &mut AppController,
    generation: u64,
    identifier: &str,
    open: bool,
) -> ArchiveJob {
    let job = ArchiveJob {
        generation,
        kind: ArchiveRequest::Details {
            identifier: identifier.into(),
            open,
        },
        due: Instant::now() + Duration::from_secs(60),
    };
    controller.archive_org.generation = generation;
    controller.archive_org.pending = Some(job.clone());
    job
}

#[test]
fn dispatched_navigation_persists_independent_query_and_parent_catalogue_row() {
    let (_directory, mut controller) = controller();
    controller.archive_org_search_query = "archive jazz".into();
    controller.youtube_search_query = "youtube music".into();
    for identifier in ["first", "second"] {
        let details = details(identifier);
        controller.archive_org.items.push(details.item.clone());
        controller
            .archive_org
            .cache
            .push_back((identifier.into(), Ok(details)));
    }
    controller.populate_archive_org();
    controller.dispatch(UiAction::MoveSelection(1));
    assert_eq!(controller.view.selected, 1);
    controller.dispatch(UiAction::ActivateSelection);
    controller.dispatch(UiAction::SelectRow(0));
    assert!(controller.save_session());
    let saved = controller.store.session().unwrap().unwrap();
    assert_eq!(saved.archive_org_search_text, "archive jazz");
    assert_eq!(saved.archive_org_selected_row, Some(1));
    assert_eq!(controller.youtube_search_query, "youtube music");
    controller.dispatch(UiAction::GoBack);
    assert_eq!(controller.view.selected, 1);
    assert!(controller.archive_org.active.is_none());
}

#[test]
fn selecting_a_cached_row_revokes_a_different_pending_open() {
    let (_directory, mut controller) = controller();
    for identifier in ["first", "second"] {
        let details = details(identifier);
        controller.archive_org.items.push(details.item.clone());
        controller
            .archive_org
            .cache
            .push_back((identifier.into(), Ok(details)));
    }
    controller.populate_archive_org();
    let job = pending(&mut controller, 1, "first", true);
    controller.dispatch(UiAction::SelectRow(1));
    controller.handle_archive_response(job, Ok(ArchiveResponse::Details(details("first"))));
    assert!(controller.archive_org.active.is_none());
    assert_eq!(controller.view.selected, 1);
}

#[test]
fn catalogue_container_is_not_playable_but_its_tracks_are_distinct() {
    let (_directory, mut controller) = controller();
    let details = details("fixture");
    controller.archive_org.items.push(details.item.clone());
    controller
        .archive_org
        .cache
        .push_back(("fixture".into(), Ok(Arc::clone(&details))));
    controller.populate_archive_org();
    assert!(controller.selected_archive_org_queue_item().is_err());
    controller.activate_archive_org_selection();
    assert_eq!(controller.view.rows.len(), 2);
    let first = controller.selected_archive_org_queue_item().unwrap();
    controller.view.selected = 1;
    let second = controller.selected_archive_org_queue_item().unwrap();
    assert_ne!(first.media.id, second.media.id);
    assert!(second.playback_location.ends_with("02.opus"));
    assert!(controller.go_back_archive_org());
    assert_eq!(controller.view.rows.len(), 1);
}

#[test]
fn stale_open_cannot_replace_current_navigation_or_another_tab() {
    let (_directory, mut controller) = controller();
    let stale = pending(&mut controller, 1, "old", true);
    pending(&mut controller, 2, "new", false);
    controller.view.screen = Screen::Playlists;
    controller.view.rows = vec![RowView {
        title: "Keep me".into(),
        ..RowView::default()
    }];
    controller.handle_archive_response(stale, Ok(ArchiveResponse::Details(details("old"))));
    assert!(controller.archive_org.active.is_none());
    assert_eq!(
        controller.archive_org.pending.as_ref().unwrap().generation,
        2
    );
    assert_eq!(controller.view.rows[0].title, "Keep me");
}

#[test]
fn escape_cancels_pending_open_even_when_http_finishes_later() {
    let (_directory, mut controller) = controller();
    let job = pending(&mut controller, 1, "fixture", true);
    assert!(controller.go_back_archive_org());
    controller.handle_archive_response(job, Ok(ArchiveResponse::Details(details("fixture"))));
    assert!(controller.archive_org.active.is_none());
}

#[test]
fn explicit_open_retries_a_cached_failure_without_automatic_retry_loops() {
    struct Offline;
    impl crate::providers::archive_org::ArchiveOrgTransport for Offline {
        fn fetch(
            &self,
            _: &url::Url,
            _: usize,
        ) -> Result<Vec<u8>, crate::providers::ProviderError> {
            Err(crate::providers::ProviderError::HttpStatus(503))
        }
    }
    let (_directory, mut controller) = controller();
    controller.archive_org.client = ArchiveOrgClient::with_transport(Arc::new(Offline));
    controller
        .archive_org
        .items
        .push(details("fixture").item.clone());
    controller
        .archive_org
        .cache
        .push_back(("fixture".into(), Err("temporary failure".into())));
    controller.update_archive_org_detail();
    assert!(
        controller.archive_org.pending.is_none(),
        "passive redraw must not retry"
    );
    controller.activate_archive_org_selection();
    assert_eq!(
        controller.view.search_activity,
        Some(SearchActivity::ArchiveOrg)
    );
    let job = controller
        .archive_org
        .pending
        .clone()
        .expect("explicit retry requested");
    assert!(matches!(
        job.kind,
        ArchiveRequest::Details { open: true, .. }
    ));
    controller.handle_archive_response(job, Ok(ArchiveResponse::Details(details("fixture"))));
    assert!(controller.archive_org.active.is_some());
    assert_eq!(controller.view.search_activity, None);
}

#[test]
fn explicit_open_errors_are_visible_but_passive_and_stale_errors_are_not_modal() {
    let (_directory, mut controller) = controller();
    let passive = pending(&mut controller, 1, "fixture", false);
    controller.handle_archive_response(passive, Err("metadata exceeds limit".into()));
    assert!(controller.view.error_popup.is_none());
    let stale = pending(&mut controller, 2, "old", true);
    pending(&mut controller, 3, "fixture", true);
    controller.handle_archive_response(stale, Err("old error".into()));
    assert!(controller.view.error_popup.is_none());
    let hidden = pending(&mut controller, 4, "fixture", true);
    controller.view.screen = Screen::Local;
    controller.handle_archive_response(hidden, Err("hidden error".into()));
    assert!(controller.view.error_popup.is_none());
    controller.view.screen = Screen::ArchiveOrg;
    let explicit = pending(&mut controller, 5, "fixture", true);
    controller.handle_archive_response(explicit, Err("metadata exceeds limit".into()));
    let popup = controller
        .view
        .error_popup
        .as_ref()
        .expect("visible open error");
    assert_eq!(popup.title, "Could not open archive.org item");
    assert_eq!(popup.report, "metadata exceeds limit");
    assert!(!popup.reportable);
}

#[test]
fn reviews_work_without_youtube_and_late_response_cannot_reopen_popup() {
    let (_directory, mut controller) = controller();
    let details = details("fixture");
    controller.youtube_video_comments_supported = false;
    controller.view.video_comments_available = false;
    controller.archive_org.active = Some(Arc::clone(&details));
    controller.open_archive_org_comments();
    let popup = controller.view.video_comments_popup.as_ref().unwrap();
    assert_eq!(popup.source, SourceKind::ArchiveOrg);
    assert_eq!(popup.state, VideoCommentsPopupState::Empty);
    controller.view.video_comments_popup = None;
    controller.complete_archive_comments(&details);
    assert!(controller.view.video_comments_popup.is_none());
}

#[test]
fn search_counts_survive_metadata_which_does_not_repeat_them() {
    let (_directory, mut controller) = controller();
    let details = details("fixture");
    let mut summary = details.item.clone();
    summary.favorite_count = Some(727);
    controller.archive_org.items.push(summary);
    controller
        .archive_org
        .cache
        .push_back(("fixture".into(), Ok(details)));
    controller.update_archive_org_detail();
    assert_eq!(controller.view.details.as_ref().unwrap().likes, "727");
}

#[test]
fn selection_coalescing_upgrades_an_existing_fetch_to_explicit_open() {
    let (_directory, mut controller) = controller();
    let job = pending(&mut controller, 1, "fixture", false);
    controller.queue_archive_request(
        ArchiveRequest::Details {
            identifier: "fixture".into(),
            open: true,
        },
        false,
    );
    assert_eq!(controller.archive_org.generation, 1);
    assert!(controller.archive_org.worker.is_none());
    controller.handle_archive_response(job, Ok(ArchiveResponse::Details(details("fixture"))));
    assert!(controller.archive_org.active.is_some());
}

#[test]
fn metadata_cache_is_bounded_even_for_stale_navigation() {
    let (_directory, mut controller) = controller();
    for generation in 1..=20 {
        let identifier = format!("item-{generation}");
        let job = pending(&mut controller, generation, &identifier, false);
        controller.archive_org.pending = None;
        controller.handle_archive_response(job, Ok(ArchiveResponse::Details(details(&identifier))));
    }
    assert_eq!(controller.archive_org.cache.len(), 8);
    assert_eq!(controller.archive_org.cache.back().unwrap().0, "item-20");
}
