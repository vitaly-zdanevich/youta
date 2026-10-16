//! Selection-owned VK album and artist navigation regression tests.

use super::*;

/// Keeps catalogue requests queued behind an owned worker without network access.
fn controller() -> AppController {
    let mut app = AppController::new(
        Config::for_dir("/tmp/youta-vk-browse-fixture"),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    app.show_screen(Screen::Vk);
    app.vk.query = "fixture search".into();
    app.view.search_query = app.vk.query.clone();
    app.config.providers.vk_cookies_file = Some(PathBuf::from("/tmp/vk-browse-cookies"));
    app.vk.cookie_file = app.config.providers.vk_cookies_file.clone();
    app.vk.items = (1..=2)
        .map(|id| {
            VkTrack::fixture_with_album(
                VkAudioId::new(7, id).unwrap(),
                "Fixture track",
                "Fixture artist",
            )
        })
        .collect();
    app.vk.items[1].album_title = Some("Fixture album".into());
    app.vk.client = Some(VkClient::fixture_for_track(&app.vk.items[1]));
    app.vk.selected = 1;
    app.populate_vk();
    app
}

/// The original URL occupies the first metadata link, followed by an internal album.
#[test]
fn vk_album_link_is_clickable_and_original_url_has_no_label() {
    let app = controller();
    let track = &app.vk.items[1];
    let details = app.view.details.as_ref().unwrap();
    assert!(details.description.is_empty());
    assert_eq!(details.links[0].url, track.id.webpage_url().as_str());
    assert_eq!(
        details.links[0].presentation,
        DetailLinkPresentation::UrlOnly
    );
    let album = details
        .links
        .iter()
        .find(|link| {
            matches!(
                link.internal_target,
                Some(DetailLinkInternalTarget::VkAlbum(_))
            )
        })
        .unwrap();
    assert_eq!(album.label, "Fixture album");
    assert_eq!(album.url, track.album_webpage_url().unwrap().as_str());
    assert!(!serde_json::to_string(details).unwrap().contains("hash"));
}

/// A supplied public destination must still belong to the selected track.
#[test]
fn vk_album_navigation_rejects_stale_targets_and_restores_search() {
    let mut app = controller();
    let url = app.vk.items[1].album_webpage_url().unwrap().to_string();
    app.open_vk_album("https://vk.ru/music/album/9_999".into());
    assert!(app.vk.back.is_empty());
    app.open_vk_album(url.clone());
    assert_eq!(app.vk.back.len(), 1);
    assert!(matches!(
        app.vk.pending_browse.as_ref().unwrap().kind,
        BrowseKind::Album
    ));
    assert_eq!(app.vk.route.as_ref().unwrap().0, url);
    let generation = app.vk.generation;
    assert!(app.back_vk_catalogue());
    assert!(app.vk.pending_browse.is_none());
    assert_eq!(app.view.search_query, "fixture search");
    assert_eq!(app.view.selected, 1);
    assert_eq!(app.vk.items.len(), 2);
    assert!(app.vk.route.is_none());
    app.apply_vk_page(
        generation,
        false,
        Ok(VkPage {
            tracks: Vec::new(),
            next: None,
        }),
    );
    assert_eq!(
        app.vk.items.len(),
        2,
        "late album completion cannot erase restored search"
    );
    assert!(!app.back_vk_catalogue());
}

/// Parent routes cannot survive switching the configured cookie-file owner.
#[test]
fn vk_cookie_change_discards_private_album_navigation() {
    let mut app = controller();
    app.open_vk_album(app.vk.items[1].album_webpage_url().unwrap().to_string());
    app.config.providers.vk_cookies_file = Some(PathBuf::from("/tmp/vk-other-cookies"));
    app.refresh_vk_cookie_owner();
    assert!(app.vk.back.is_empty());
    assert!(app.vk.route.is_none());
    assert!(app.vk.pending_browse.is_none());
}

/// Failed query parsing cannot discard the owner of an already queued album request.
#[test]
fn invalid_vk_query_preserves_the_current_album_and_back_route() {
    let mut app = controller();
    app.open_vk_album(app.vk.items[1].album_webpage_url().unwrap().to_string());
    let route = app.vk.route.clone();
    let generation = app.vk.generation;
    app.submit_vk_search("profile:".into());
    assert_eq!(app.vk.route, route);
    assert_eq!(app.vk.generation, generation);
    assert_eq!(app.vk.back.len(), 1);
    assert!(app.vk.pending_browse.is_some());
}

/// A focused playing row cannot inherit another catalogue's Back ancestors.
#[test]
fn vk_now_playing_snapshot_swaps_browse_history_reversibly() {
    let mut app = controller();
    let item = queue_item(&app.vk.items[0]);
    app.retain_vk_track(app.vk.items[0].clone());
    let mut snapshot = app.vk_navigation_snapshot(&item).unwrap();
    app.open_vk_album(app.vk.items[1].album_webpage_url().unwrap().to_string());
    assert_eq!(app.vk.back.len(), 1);
    app.swap_vk_navigation(&mut snapshot);
    assert!(app.vk.back.is_empty());
    app.swap_vk_navigation(&mut snapshot);
    assert_eq!(app.vk.back.len(), 1);
}

/// Artist navigation follows an exact provider identity and preserves Back.
#[test]
fn vk_artist_link_and_action_open_the_selected_artist() {
    let mut app = controller();
    app.vk.items[1] =
        VkTrack::fixture_with_artist(VkAudioId::new(7, 2).unwrap(), "Track", "Artist");
    app.update_vk_detail();
    let url = app.vk.items[1].artist_webpage_url().unwrap().to_string();
    let details = app.view.details.as_ref().unwrap();
    assert!(
        details
            .links
            .iter()
            .any(|link| link.internal_target
                == Some(DetailLinkInternalTarget::VkArtist(url.clone())))
    );
    app.dispatch(UiAction::OpenVkArtist(url));
    assert!(matches!(
        app.vk.pending_browse.as_ref().unwrap().kind,
        BrowseKind::Artist
    ));
    assert!(app.back_vk_catalogue());
    assert_eq!(app.view.selected, 1);
}

/// Publishing capabilities exclude continuation rows and transient playback locators.
#[cfg(any(feature = "commons-upload", feature = "archive-upload"))]
#[test]
fn vk_export_controls_require_a_finite_canonical_selection() {
    let mut app = controller();
    app.refresh_selected_playlist_state();
    #[cfg(feature = "commons-upload")]
    assert!(app.view.commons_upload_available);
    #[cfg(feature = "archive-upload")]
    assert!(app.view.archive_upload_available);
    let mut item = app.selected_vk_queue_item().unwrap();
    assert!(export_supported(&item));
    item.playback_location = "https://cdn.example/signed.mp3?token=secret".into();
    assert!(!export_supported(&item));
    app.view.selected = app.vk.items.len();
    app.update_vk_detail();
    app.refresh_selected_playlist_state();
    #[cfg(feature = "commons-upload")]
    assert!(!app.view.commons_upload_available);
    #[cfg(feature = "archive-upload")]
    assert!(!app.view.archive_upload_available);
}

/// Merely opening upload review does not resolve audio or publish to either service.
#[cfg(feature = "commons-upload")]
#[test]
fn vk_commons_review_keeps_the_original_source_and_uses_native_staging() {
    let mut app = controller();
    let page = app.vk.items[1].id.webpage_url();
    app.open_commons_upload();
    let selection = app.commons_upload_selection.as_ref().unwrap();
    assert_eq!(selection.media.webpage_url, page);
    assert!(
        matches!(&selection.source, CommonsAudioSource::VkSession(source) if source.webpage_url() == page)
    );
    assert!(app.commons_upload_thread.is_none());
    assert!(app.vk.worker.is_none());
}

/// Archive only receives canonical provenance and cannot request a VK video upload.
#[cfg(feature = "archive-upload")]
#[test]
fn vk_archive_review_preserves_canonical_provenance_and_audio_only_capability() {
    let mut app = controller();
    app.config.archive_upload.upload_video = true;
    let page = app.vk.items[1].id.webpage_url();
    app.dispatch(UiAction::OpenArchiveUpload);
    let popup = app.view.archive_upload_popup.as_ref().unwrap();
    assert_eq!(
        popup.draft.source,
        crate::archive_upload::ArchiveUploadSource::Vk
    );
    assert_eq!(popup.draft.source_url, page.as_str());
    assert!(!popup.video_available);
    assert!(!popup.draft.upload_video);
    assert_eq!(popup.phase, crate::view::ArchiveUploadPhase::Review);
    assert!(app.vk.worker.is_none());
}

/// Old now-playing snapshots cannot resurrect private rows after a cookie-owner change.
#[test]
fn vk_cookie_change_revokes_the_accepted_now_playing_snapshot() {
    let mut app = controller();
    let item = queue_item(&app.vk.items[0]);
    app.remember_now_playing_context(&item);
    app.current_media = Some(item.media.id.clone());
    app.config.providers.vk_cookies_file = Some(PathBuf::from("/tmp/vk-other-session"));
    app.refresh_vk_cookie_owner();
    assert!(!app.reveal_retained_now_playing(&item));
    assert!(app.vk.items.is_empty());
}
