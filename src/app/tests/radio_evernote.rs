//! Recording completion offers are local, review-first, and independent of live Radio selection.

use super::*;

/// Completes a mock capture and returns the newly published, collision-safe pathname.
fn finish_mock_recording(app: &mut AppController) -> PathBuf {
    let previous = std::fs::read_dir(app.config.downloads_dir())
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    app.toggle_radio_recording();
    let staging = &app.active_radio_recording.as_ref().unwrap().staging_path;
    std::fs::write(staging, b"completed encoded packets").unwrap();
    app.toggle_radio_recording();
    std::fs::read_dir(app.config.downloads_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| !previous.contains(path))
        .unwrap()
}

/// Writes mock encoded packets only into the recording's private staging path.
fn completed_radio_fixture(token: bool) -> (tempfile::TempDir, AppController, PathBuf) {
    let temporary = crate::test_support::canonical_tempdir("recording Evernote offer");
    let (mut app, _, _) = radio_recording_controller(&temporary);
    app.config.providers.evernote_auth_token = token.then(|| "fixture-token".into());
    let published = finish_mock_recording(&mut app);
    (temporary, app, published)
}

#[test]
fn radio_evernote_live_selection_and_direct_action_are_unavailable() {
    let temporary = crate::test_support::canonical_tempdir("live radio Evernote guard");
    let (mut app, _, _) = radio_recording_controller(&temporary);
    let (requests, captured) = unbounded();
    app.provider_requests = Some(requests);
    app.refresh_selected_playlist_state();
    assert!(
        !app.view.evernote_available,
        "a live stream is not a finite Evernote attachment"
    );
    app.dispatch(UiAction::OpenEvernoteNote);
    assert!(app.evernote_selection.is_none());
    assert!(app.view.evernote_popup.is_none());
    assert!(app.view.evernote_credentials_popup.is_none());
    assert!(app.evernote_thread.is_none());
    assert!(
        captured.try_recv().is_err(),
        "reject before metadata/network work"
    );
}

#[test]
fn radio_evernote_completed_file_opens_review_without_uploading() {
    let (_temporary, app, published) = completed_radio_fixture(true);
    let selection = app
        .evernote_selection
        .as_ref()
        .expect("offer retains completed file");
    assert_eq!(selection.media.id.source, SourceKind::Local);
    assert!(matches!(&selection.source, OpusAudioSource::LocalFile(path) if path == &published));
    assert_eq!(
        local_path_from_media_id(&selection.media.id).as_ref(),
        Some(&published)
    );
    let popup = app
        .view
        .evernote_popup
        .as_ref()
        .expect("review is the offer");
    assert_eq!(popup.phase, EvernoteNotePhase::Review);
    assert!(
        popup.draft.source_url.is_empty(),
        "never expose a local file URL in uploaded note metadata"
    );
    assert!(!popup.captions_available);
    assert!(popup.draft.title.contains("Radio Swiss Classic"));
    assert!(
        app.evernote_thread.is_none(),
        "only explicit Submit can prepare/upload audio"
    );
    assert_eq!(app.evernote_generation, 0);
}

#[test]
fn radio_evernote_missing_token_still_offers_review_before_credentials() {
    let (_temporary, mut app, published) = completed_radio_fixture(false);
    assert!(
        app.view.evernote_credentials_popup.is_none(),
        "do not solicit credentials before accepting the offer"
    );
    let popup = app
        .view
        .evernote_popup
        .as_mut()
        .expect("review offered without an account");
    popup.draft.title = "My edited recording title".into();
    popup.draft.body = "Private review edits".into();
    popup.draft.tags = "radio, archive".into();
    let edited = popup.draft.clone();
    app.submit_evernote_note();
    assert!(app.view.evernote_popup.is_none());
    app.view
        .evernote_credentials_popup
        .as_mut()
        .expect("explicit Submit requests credentials")
        .token = "fixture-token".into();
    app.submit_evernote_credentials();
    let review = app
        .view
        .evernote_popup
        .as_ref()
        .expect("return to review, not upload");
    assert_eq!(review.draft, edited);
    assert!(
        matches!(&app.evernote_selection.as_ref().unwrap().source, OpusAudioSource::LocalFile(path) if path == &published)
    );
    assert!(app.evernote_thread.is_none());
    app.dispatch(UiAction::DismissEvernoteNote);
    assert!(app.evernote_selection.is_none());
}

#[test]
fn radio_evernote_modal_defers_exact_completed_recording_until_tick() {
    let temporary = crate::test_support::canonical_tempdir("deferred recording offer");
    let (mut app, _, _) = radio_recording_controller(&temporary);
    app.view.help_open = true;
    let published = finish_mock_recording(&mut app);
    assert_eq!(app.pending_radio_evernote_offers.len(), 1);
    assert!(app.evernote_selection.is_none());
    assert!(app.view.evernote_popup.is_none());
    app.view.help_open = false;
    app.tick();
    assert!(app.pending_radio_evernote_offers.is_empty());
    assert!(matches!(&app.evernote_selection.as_ref().unwrap().source,
        OpusAudioSource::LocalFile(path) if path == &published));
    assert!(app.view.evernote_popup.is_some());
    assert!(app.evernote_thread.is_none());
}

#[test]
fn radio_evernote_fullscreen_artwork_defers_review_until_collapsed() {
    let temporary = crate::test_support::canonical_tempdir("artwork recording offer");
    let (mut app, _, _) = radio_recording_controller(&temporary);
    app.view.details.as_mut().unwrap().thumbnail_url =
        Some(url::Url::parse("https://images.example.test/station.png").unwrap());
    app.dispatch(UiAction::ToggleThumbnailExpansion);
    assert!(app.view.expanded_thumbnail_available());
    let published = finish_mock_recording(&mut app);
    assert!(
        app.view.evernote_popup.is_none(),
        "artwork keeps modal ownership"
    );
    assert!(app.view.evernote_credentials_popup.is_none());
    assert!(app.evernote_selection.is_none());
    assert_eq!(app.pending_radio_evernote_offers.len(), 1);
    app.dispatch(UiAction::ToggleThumbnailExpansion);
    assert!(!app.view.expanded_thumbnail_available());
    app.tick();
    assert!(app.pending_radio_evernote_offers.is_empty());
    assert!(app.view.evernote_popup.is_some());
    assert!(matches!(&app.evernote_selection.as_ref().unwrap().source,
        OpusAudioSource::LocalFile(path) if path == &published));
    assert!(app.evernote_thread.is_none());
}

#[test]
fn radio_evernote_existing_edited_draft_is_not_overwritten_by_new_recording_or_action() {
    let (_temporary, mut app, first) = completed_radio_fixture(true);
    app.view.evernote_popup.as_mut().unwrap().draft.body = "Keep these edits".into();
    let draft = app.view.evernote_popup.as_ref().unwrap().draft.clone();
    let second = finish_mock_recording(&mut app);
    assert_ne!(first, second);
    assert!(first.exists());
    app.dispatch(UiAction::OpenEvernoteNote);
    assert_eq!(app.view.evernote_popup.as_ref().unwrap().draft, draft);
    assert!(matches!(&app.evernote_selection.as_ref().unwrap().source,
        OpusAudioSource::LocalFile(path) if path == &first));
    assert_eq!(app.pending_radio_evernote_offers.len(), 1);
    app.dispatch(UiAction::DismissEvernoteNote);
    app.maybe_offer_completed_radio_recording_to_evernote();
    assert!(matches!(&app.evernote_selection.as_ref().unwrap().source,
        OpusAudioSource::LocalFile(path) if path == &second));
    assert!(app.pending_radio_evernote_offers.is_empty());
    assert!(app.evernote_thread.is_none());
}

#[test]
fn radio_evernote_active_worker_defers_offer_without_replacing_the_worker() {
    let temporary = crate::test_support::canonical_tempdir("busy Evernote recording offer");
    let (mut app, _, _) = radio_recording_controller(&temporary);
    let (release, blocked) = unbounded::<()>();
    let worker = thread::spawn(move || {
        let _ = blocked.recv_timeout(Duration::from_secs(5));
    });
    let worker_id = worker.thread().id();
    app.evernote_thread = Some(worker);
    let published = finish_mock_recording(&mut app);
    assert_eq!(
        app.evernote_thread.as_ref().unwrap().thread().id(),
        worker_id
    );
    assert!(app.view.evernote_popup.is_none());
    assert!(app.evernote_selection.is_none());
    assert_eq!(app.pending_radio_evernote_offers.len(), 1);
    release.send(()).unwrap();
    app.evernote_thread.take().unwrap().join().unwrap();
    app.maybe_offer_completed_radio_recording_to_evernote();
    assert!(matches!(&app.evernote_selection.as_ref().unwrap().source,
        OpusAudioSource::LocalFile(path) if path == &published));
}

#[test]
fn radio_evernote_shutdown_publishes_without_offer_or_credentials() {
    let temporary = crate::test_support::canonical_tempdir("shutdown recording without offer");
    let (mut app, _, _) = radio_recording_controller(&temporary);
    app.toggle_radio_recording();
    std::fs::write(
        &app.active_radio_recording.as_ref().unwrap().staging_path,
        b"completed encoded packets",
    )
    .unwrap();
    assert!(app.shutdown());
    assert_eq!(
        std::fs::read_dir(app.config.downloads_dir())
            .unwrap()
            .count(),
        1
    );
    assert!(app.pending_radio_evernote_offers.is_empty());
    assert!(app.evernote_selection.is_none());
    assert!(app.view.evernote_popup.is_none());
    assert!(app.view.evernote_credentials_popup.is_none());
    assert!(app.evernote_thread.is_none());
}

#[test]
fn radio_evernote_failed_empty_missing_and_discarded_captures_never_offer() {
    for failure in ["empty", "missing", "stop", "discard"] {
        let temporary = crate::test_support::canonical_tempdir("failed recording without offer");
        let (mut app, backend, _) = radio_recording_controller(&temporary);
        app.toggle_radio_recording();
        let staging = app
            .active_radio_recording
            .as_ref()
            .unwrap()
            .staging_path
            .clone();
        if failure == "missing" {
            assert!(
                !staging.exists(),
                "the mock backend has not written packets"
            );
        } else {
            let bytes = if failure == "empty" {
                &b""[..]
            } else {
                &b"packets"[..]
            };
            std::fs::write(&staging, bytes).unwrap();
        }
        if failure == "stop" {
            backend.lock().unwrap().end_before_command_error = true;
        }
        if failure == "discard" {
            app.discard_active_radio_recording();
        } else {
            app.toggle_radio_recording();
        }
        assert!(app.pending_radio_evernote_offers.is_empty(), "{failure}");
        assert!(app.evernote_selection.is_none(), "{failure}");
        assert!(app.view.evernote_popup.is_none(), "{failure}");
        assert!(app.view.evernote_credentials_popup.is_none(), "{failure}");
        assert!(app.evernote_thread.is_none(), "{failure}");
    }
}

#[test]
fn radio_evernote_deferred_missing_or_replaced_file_is_not_offered() {
    for replace in [false, true] {
        let temporary = crate::test_support::canonical_tempdir("changed recording offer");
        let (mut app, _, _) = radio_recording_controller(&temporary);
        app.view.help_open = true;
        let published = finish_mock_recording(&mut app);
        if replace {
            std::fs::write(&published, b"different replacement bytes").unwrap();
        } else {
            std::fs::remove_file(&published).unwrap();
        }
        app.view.help_open = false;
        app.maybe_offer_completed_radio_recording_to_evernote();
        assert!(app.pending_radio_evernote_offers.is_empty());
        assert!(app.evernote_selection.is_none());
        assert!(app.view.evernote_popup.is_none());
        assert!(app.evernote_thread.is_none());
    }
}

#[test]
fn radio_evernote_cancelled_credentials_clear_the_draft_without_uploading() {
    let (_temporary, mut app, published) = completed_radio_fixture(false);
    app.view.evernote_popup.as_mut().unwrap().draft.body = "Edited draft".into();
    app.submit_evernote_note();
    assert!(app.evernote_credentials_draft.is_some());
    app.dispatch(UiAction::DismissEvernoteCredentials);
    assert!(app.evernote_credentials_draft.is_none());
    assert!(app.evernote_selection.is_none());
    assert!(app.view.evernote_popup.is_none());
    assert!(app.view.evernote_credentials_popup.is_none());
    assert!(app.evernote_thread.is_none());
    assert!(
        published.exists(),
        "cancelling export never removes the recording"
    );
}

#[test]
fn radio_evernote_offer_fifo_is_bounded_without_removing_recordings() {
    let temporary = crate::test_support::canonical_tempdir("bounded recording offer queue");
    let (mut app, _, _) = radio_recording_controller(&temporary);
    app.view.help_open = true;
    let paths = (0..10)
        .map(|_| finish_mock_recording(&mut app))
        .collect::<Vec<_>>();
    assert_eq!(app.pending_radio_evernote_offers.len(), 8);
    app.view.help_open = false;
    for expected in &paths[2..] {
        app.maybe_offer_completed_radio_recording_to_evernote();
        assert!(matches!(&app.evernote_selection.as_ref().unwrap().source,
            OpusAudioSource::LocalFile(path) if path == expected));
        app.dispatch(UiAction::DismissEvernoteNote);
    }
    assert!(app.pending_radio_evernote_offers.is_empty());
    assert!(paths.iter().all(|path| path.exists()));
    assert!(app.evernote_thread.is_none());
}
