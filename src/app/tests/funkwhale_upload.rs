//! Review-only Funkwhale exports keep original track pages and selected ownership.

use super::*;

/// Uses the provider's slashless track identity without starting any media request.
fn selected_track() -> (AppController, tempfile::TempDir) {
    let directory = crate::test_support::canonical_tempdir("Funkwhale upload selection");
    let mut app = AppController::new(
        Config::for_dir(directory.path()),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    app.view.screen = Screen::Search;
    let page = url::Url::parse("https://pod.example/prefix/library/tracks/27").unwrap();
    app.resolved_direct = Some(ResolvedDirectMedia {
        source: SourceKind::Funkwhale,
        external_id: page.to_string(),
        title: "Original selected track".to_owned(),
        row_subtitle: String::new(),
        description: "Summary".to_owned(),
        license: "unknown".to_owned(),
        published: None,
        artwork_url: None,
        duration_seconds: Some(123),
        playback_url: Some(page.clone()),
        webpage_url: Some(page.clone()),
        status_line: String::new(),
    });
    app.view.details = Some(DetailView {
        media_id: Some(MediaId::new(SourceKind::Funkwhale, page.as_str())),
        title: "Original selected track".to_owned(),
        description: "Exact selected provider description".to_owned(),
        channel_name: "Original artist".to_owned(),
        license: "CC BY 4.0".to_owned(),
        webpage_url: Some(page),
        channel_webpage_url: Some(
            url::Url::parse("https://pod.example/prefix/library/artists/9/").unwrap(),
        ),
        ..DetailView::default()
    });
    (app, directory)
}

/// Opening a review captures only the stable page; it must not resolve or upload audio.
#[cfg(feature = "commons-upload")]
#[test]
fn funkwhale_commons_review_captures_exact_selected_metadata_without_fetching() {
    let (mut app, _directory) = selected_track();
    app.refresh_selected_playlist_state();
    assert!(app.view.commons_upload_available);
    app.dispatch(UiAction::OpenCommonsUpload);
    assert!(app.commons_upload_thread.is_none());
    let selection = app
        .commons_upload_selection
        .as_ref()
        .expect("review owns its source");
    assert_eq!(selection.media.creator.as_deref(), Some("Original artist"));
    assert_eq!(
        selection.media.description.as_deref(),
        Some("Exact selected provider description")
    );
    match &selection.source {
        CommonsAudioSource::Funkwhale(page) => {
            assert_eq!(page.as_str(), selection.media.id.external_id)
        }
        _ => panic!("Funkwhale cannot be delegated as an HTML public-page download"),
    }
}

/// Completing credentials cannot replace the frozen artist link with a later row's artist.
#[cfg(feature = "commons-upload")]
#[test]
fn funkwhale_commons_credentials_keep_the_original_selected_artist_link() {
    let (mut app, _directory) = selected_track();
    app.open_commons_upload();
    app.view.commons_credentials_popup = Some(CommonsCredentialsPopupView {
        username: "fixture-user".to_owned(),
        password: "fixture-password".to_owned(),
        auth_method: WikimediaCommonsAuthMethod::BotPassword,
        password_selected: false,
        credentials_path: app.config.credentials_file().display().to_string(),
        validation_error: None,
    });
    app.view.details.as_mut().unwrap().channel_webpage_url =
        Some(url::Url::parse("https://wrong.example/artist").unwrap());
    app.submit_commons_credentials();
    let draft = &app
        .view
        .commons_upload_popup
        .as_ref()
        .expect("completed credentials open review only")
        .draft;
    assert_eq!(
        draft.author,
        "[https://pod.example/prefix/library/artists/9/ Original artist]"
    );
    assert!(app.commons_upload_thread.is_none());
}

/// Existing Archive reviews remain audio-only and preserve canonical public provenance.
#[cfg(feature = "archive-upload")]
#[test]
fn funkwhale_archive_review_is_audio_only_and_not_published_when_opened() {
    let (mut app, _directory) = selected_track();
    app.config.archive_upload.upload_video = true;
    app.refresh_selected_playlist_state();
    assert!(app.view.archive_upload_available);
    app.dispatch(UiAction::OpenArchiveUpload);
    let popup = app
        .view
        .archive_upload_popup
        .as_ref()
        .expect("Archive review");
    assert_eq!(
        popup.draft.source,
        crate::archive_upload::ArchiveUploadSource::Funkwhale
    );
    assert_eq!(
        popup.draft.source_url,
        "https://pod.example/prefix/library/tracks/27"
    );
    assert_eq!(popup.draft.creator, "Original artist");
    assert_eq!(
        popup.draft.description,
        "Exact selected provider description"
    );
    assert!(!popup.video_available);
    assert!(!popup.draft.upload_video);
    assert_eq!(popup.phase, crate::view::ArchiveUploadPhase::Review);
    app.dispatch(UiAction::ToggleArchiveUploadVideo);
    assert!(
        !app.view
            .archive_upload_popup
            .as_ref()
            .unwrap()
            .draft
            .upload_video
    );
}

/// Mismatched page, replay identity or transient audio cannot expose a publishing action.
#[test]
fn funkwhale_upload_capability_rejects_noncanonical_and_transient_locators() {
    let (mut app, _directory) = selected_track();
    let canonical = app.resolved_direct.clone().unwrap();
    for invalid in [
        "https://pod.example/api/v2/listen/token/?token=secret",
        "https://pod.example/prefix/library/artists/27/",
        "https://user:password@pod.example/library/tracks/27/",
        "https://pod.example/library/tracks/27/?token=secret",
        "https://other-pod.example/prefix/library/tracks/27",
        "https://pod.example/other-prefix/library/tracks/27",
    ] {
        app.resolved_direct = Some(canonical.clone());
        app.resolved_direct.as_mut().unwrap().playback_url =
            Some(url::Url::parse(invalid).unwrap());
        app.refresh_selected_playlist_state();
        #[cfg(feature = "commons-upload")]
        assert!(!app.view.commons_upload_available, "{invalid}");
        #[cfg(feature = "archive-upload")]
        assert!(!app.view.archive_upload_available, "{invalid}");
    }
}

/// Publishing capability is finite-audio-only and requires an original track page.
#[test]
fn funkwhale_upload_capability_rejects_non_audio_and_noncanonical_equal_locators() {
    let (app, _directory) = selected_track();
    let original = app.selected_queue_item().unwrap();
    assert!(super::super::funkwhale_upload::supported(&original));
    for kind in [
        MediaKind::Video,
        MediaKind::LiveStream,
        MediaKind::PodcastEpisode,
    ] {
        let mut item = original.clone();
        item.media.kind = kind;
        assert!(!super::super::funkwhale_upload::supported(&item));
    }
    for page in [
        "https://pod.example/library/tracks/027/",
        "https://pod.example/library/tracks/027",
        "https://pod.example/library/tracks/0",
        "https://pod.example/library/tracks/27//",
        "https://pod.example/library/artists/27/",
        "https://pod.example/library/tracks/27/?token=secret",
        "https://pod.example/library/tracks/27#fragment",
    ] {
        let mut item = original.clone();
        item.media.id.external_id = page.to_owned();
        item.media.webpage_url = url::Url::parse(page).unwrap();
        item.playback_location = page.to_owned();
        assert!(!super::super::funkwhale_upload::supported(&item), "{page}");
    }
}
