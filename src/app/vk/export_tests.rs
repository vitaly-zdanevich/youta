//! Exact authenticated export snapshots never fall back to title-search guesses.

use super::*;

/// A later-page or album-only recording is retained without any search request.
fn controller() -> AppController {
    let mut app = AppController::new(
        Config::for_dir("/tmp/youta-vk-exact-export-fixture"),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    let cookie = std::env::temp_dir().join("youta-vk-exact-export-cookies");
    app.config.providers.vk_cookies_file = Some(cookie.clone());
    let track = VkTrack::fixture(
        VkAudioId::new(-7, 999).unwrap(),
        "Album-only recording",
        "Fixture artist",
    );
    app.vk = VkState::export_fixture(track, cookie);
    app.show_screen(Screen::Vk);
    app
}

/// Synthetic credentials use the platform's absolute temporary root without reading a cookie file.
#[test]
fn exact_vk_export_fixture_has_a_valid_platform_cookie_owner() {
    let app = controller();
    let cookie_file = app.config.providers.vk_cookies_file.as_ref().unwrap();
    assert!(
        cookie_file.is_absolute(),
        "fixture cookies require an absolute path"
    );
    assert_eq!(cookie_file.parent(), Some(std::env::temp_dir().as_path()));
    let owner = app
        .vk_export_owner()
        .expect("valid synthetic session owner");
    assert_eq!(&owner.cookie_file, cookie_file);
    assert!(app.vk_export_owner_is_current(&owner));
}

/// Export capture keeps the exact selected identity while the catalogue worker owns its client.
#[test]
fn exact_vk_export_remains_available_while_catalogue_client_is_in_flight() {
    let mut app = controller();
    let item = app.selected_vk_queue_item().unwrap();
    let original = app.capture_vk_export_source(&item).unwrap();
    assert_eq!(original.webpage_url(), item.media.webpage_url);
    let _worker_client = app.take_vk_worker_client().unwrap();
    assert!(app.vk.client.is_none());
    let captured = app.capture_vk_export_source(&item).unwrap();
    assert_eq!(captured.webpage_url(), item.media.webpage_url);
    assert!(!format!("{captured:?}").contains("fixture-action"));
    assert!(
        app.vk.worker.is_none(),
        "capturing a source performs no request"
    );
}

/// Exact scopes cannot cross credential replacement, shutdown, or mismatched public identity.
#[test]
fn exact_vk_export_rejects_stale_session_and_mismatched_item() {
    let mut app = controller();
    let mut item = app.selected_vk_queue_item().unwrap();
    let owner = app.vk_export_owner().unwrap();
    item.media.id.external_id = "-7_998".to_owned();
    assert!(app.capture_vk_export_source(&item).is_none());
    let item = app.selected_vk_queue_item().unwrap();
    app.take_vk_worker_client();
    app.config.providers.vk_cookies_file =
        Some(std::env::temp_dir().join("youta-vk-other-export-cookies"));
    assert!(!app.vk_export_owner_is_current(&owner));
    assert!(app.capture_vk_export_source(&item).is_none());
    app.refresh_vk_cookie_owner();
    assert!(app.vk.export_client.is_none());
    app.config.providers.vk_cookies_file = Some(owner.cookie_file.clone());
    app.refresh_vk_cookie_owner();
    assert!(!app.vk_export_owner_is_current(&owner));
}

/// A reviewed Commons source remains exact, but replacing the import revokes confirmation.
#[cfg(feature = "commons-upload")]
#[test]
fn vk_commons_review_captures_session_and_rejects_credential_replacement() {
    let mut app = controller();
    let page = app.selected_vk_queue_item().unwrap().media.webpage_url;
    app.open_commons_upload();
    let selection = app.commons_upload_selection.as_ref().unwrap();
    assert!(
        matches!(&selection.source, CommonsAudioSource::VkSession(source) if source.webpage_url() == page)
    );
    assert!(app.vk_export_owner_is_current(selection.vk_owner.as_ref().unwrap()));
    let draft = CommonsUploadDraft::from_media(&selection.media, selection.channel_url.as_ref());
    app.open_commons_review(draft);
    let original = app.config.providers.vk_cookies_file.clone();
    app.config.providers.vk_cookies_file = Some(std::env::temp_dir().join("vk-review-new-owner"));
    app.refresh_vk_cookie_owner();
    app.config.providers.vk_cookies_file = original;
    app.refresh_vk_cookie_owner();
    assert!(
        !app.vk_export_owner_is_current(
            app.commons_upload_selection
                .as_ref()
                .unwrap()
                .vk_owner
                .as_ref()
                .unwrap()
        )
    );
    app.submit_commons_upload();
    assert!(
        app.view
            .commons_upload_popup
            .as_ref()
            .unwrap()
            .validation_error
            .as_ref()
            .unwrap()
            .contains("VK session changed")
    );
    assert!(app.commons_upload_thread.is_none());
}

/// Credential replacement invalidates Archive review before credential discovery or publication.
#[cfg(feature = "archive-upload")]
#[test]
fn vk_archive_review_rejects_replaced_session_before_submission() {
    let mut app = controller();
    app.open_archive_upload();
    let generation = app.view.archive_upload_popup.as_ref().unwrap().generation;
    app.config.providers.vk_cookies_file = Some(std::env::temp_dir().join("vk-archive-new-owner"));
    app.submit_archive_upload(generation);
    let popup = app.view.archive_upload_popup.as_ref().unwrap();
    assert!(
        popup
            .validation_error
            .as_ref()
            .unwrap()
            .contains("VK session changed")
    );
    assert!(app.view.archive_credentials_popup.is_none());
}

/// The fake publisher observes the exact private snapshot without resolving or publishing audio.
#[cfg(feature = "archive-upload")]
struct ArchiveRecorder(Sender<(url::Url, url::Url)>);

#[cfg(feature = "archive-upload")]
impl archive_upload::ArchiveUploadService for ArchiveRecorder {
    fn discover(
        &self,
        _: &Config,
    ) -> Result<Option<crate::archive_upload::ArchiveUploadCredentials>, String> {
        Ok(Some(crate::archive_upload::ArchiveUploadCredentials::new(
            "fixture-access".to_owned(),
            "fixture-secret".to_owned(),
        )?))
    }

    fn run(
        &self,
        job: archive_upload::ArchiveUploadJob,
        _: &Arc<AtomicBool>,
        _: &mut dyn FnMut(crate::view::ArchiveUploadPhase, u64, Option<u64>),
    ) -> Result<crate::archive_upload::ArchiveUploadResult, String> {
        self.0
            .send((job.media.webpage_url, job.vk_source.unwrap().webpage_url()))
            .unwrap();
        Err("Fixture publisher performs no network requests".to_owned())
    }
}

/// A changing catalogue selection cannot substitute another recording after review opens.
#[cfg(feature = "archive-upload")]
#[test]
fn vk_archive_confirmation_keeps_exact_reviewed_track_while_worker_owns_client() {
    let mut app = controller();
    let (sender, receiver) = bounded(1);
    app.archive_upload.service = Arc::new(ArchiveRecorder(sender));
    let page = app.selected_vk_queue_item().unwrap().media.webpage_url;
    app.take_vk_worker_client();
    app.open_archive_upload();
    let generation = app.view.archive_upload_popup.as_ref().unwrap().generation;
    app.vk.items.clear();
    app.populate_vk();
    app.submit_archive_upload(generation);
    let (metadata, captured) = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(metadata, page);
    assert_eq!(captured, page);
    app.shutdown_archive_upload();
}
