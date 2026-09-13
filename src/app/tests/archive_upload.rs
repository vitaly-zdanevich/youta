//! Review, persistence, and cancellation tests with no real credential or network access.

use super::*;
use crate::app::archive_upload::{ArchiveUploadJob, ArchiveUploadService};
use crate::archive_upload::{ArchiveUploadCredentials, ArchiveUploadResult};
use crate::view::ArchiveUploadPhase;

#[derive(Default)]
struct FakeArchiveUpload {
    jobs: Mutex<Vec<(String, bool)>>,
    wait_for_cancel: AtomicBool,
    reject_credentials: AtomicBool,
    discover_credentials: AtomicBool,
    discovery_count: std::sync::atomic::AtomicUsize,
}

impl ArchiveUploadService for FakeArchiveUpload {
    fn discover(&self, _config: &Config) -> Result<Option<ArchiveUploadCredentials>, String> {
        self.discovery_count.fetch_add(1, Ordering::Relaxed);
        if self.discover_credentials.load(Ordering::Relaxed) {
            ArchiveUploadCredentials::new(
                "configured-access".to_owned(),
                "configured-secret".to_owned(),
            )
            .map(Some)
        } else {
            Ok(None)
        }
    }
    fn run(
        &self,
        job: ArchiveUploadJob,
        cancellation: &Arc<AtomicBool>,
        progress: &mut dyn FnMut(ArchiveUploadPhase, u64, Option<u64>),
    ) -> Result<ArchiveUploadResult, String> {
        self.jobs
            .lock()
            .unwrap()
            .push((job.media.id.external_id, job.draft.upload_video));
        progress(ArchiveUploadPhase::Uploading, 4, Some(8));
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.wait_for_cancel.load(Ordering::Relaxed) && !cancellation.load(Ordering::Relaxed)
        {
            assert!(
                Instant::now() < deadline,
                "controller must cancel the fake worker"
            );
            thread::sleep(Duration::from_millis(1));
        }
        if cancellation.load(Ordering::Relaxed) {
            Err("Cancelled; no fake item was published".to_owned())
        } else if self.reject_credentials.load(Ordering::Relaxed) {
            Err("Archive rejected the credentials or publication permissions".to_owned())
        } else {
            Ok(ArchiveUploadResult {
                item_url: url::Url::parse("https://archive.org/details/test-result").unwrap(),
            })
        }
    }
}

fn controller() -> (AppController, Arc<FakeArchiveUpload>, tempfile::TempDir) {
    let directory = crate::test_support::canonical_tempdir("archive upload controller");
    let config = Config::for_dir(directory.path());
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    controller.youtube_results = vec![SearchItem::Video(fixture_download_video())];
    controller.refresh_youtube_rows();
    let service = Arc::new(FakeArchiveUpload::default());
    controller.archive_upload.service = service.clone();
    (controller, service, directory)
}

fn confirm(controller: &mut AppController) {
    let generation = controller
        .view
        .archive_upload_popup
        .as_ref()
        .unwrap()
        .generation;
    controller.dispatch(UiAction::SubmitArchiveUpload(generation));
}

fn finish(controller: &mut AppController) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        controller.poll_archive_upload();
        if controller
            .view
            .archive_upload_popup
            .as_ref()
            .is_some_and(|popup| {
                matches!(
                    popup.phase,
                    ArchiveUploadPhase::Complete
                        | ArchiveUploadPhase::Failed
                        | ArchiveUploadPhase::Cancelled
                )
            })
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "bounded fake operation completed"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn archive_failed_session_credentials_can_be_replaced_in_a_new_review() {
    let (mut controller, service, _directory) = controller();
    service.reject_credentials.store(true, Ordering::Relaxed);
    controller.archive_upload.credentials = Some(
        ArchiveUploadCredentials::new("wrong-access".to_owned(), "wrong-secret".to_owned())
            .unwrap(),
    );
    controller.dispatch(UiAction::OpenArchiveUpload);
    confirm(&mut controller);
    finish(&mut controller);
    assert_eq!(
        controller.view.archive_upload_popup.as_ref().unwrap().phase,
        ArchiveUploadPhase::Failed
    );
    assert!(controller.archive_upload.credentials.is_none());
    controller.dispatch(UiAction::DismissArchiveUpload);
    controller.dispatch(UiAction::OpenArchiveUpload);
    confirm(&mut controller);
    assert!(controller.view.archive_credentials_popup.is_some());
    assert_eq!(service.jobs.lock().unwrap().len(), 1);
}

/// The credential editor consumes parent actions even when an older GUI event arrives late.
#[test]
fn archive_credential_modal_preserves_hidden_review_and_partially_entered_keys() {
    let (mut controller, service, directory) = controller();
    controller.dispatch(UiAction::OpenArchiveUpload);
    confirm(&mut controller);
    controller
        .view
        .archive_credentials_popup
        .as_mut()
        .unwrap()
        .access_key = "partially-entered".to_owned();
    let before_review = controller.view.archive_upload_popup.clone().unwrap();
    let before_credentials = controller.view.archive_credentials_popup.clone().unwrap();
    let generation = before_review.generation;
    for action in [
        UiAction::OpenArchiveUpload,
        UiAction::SelectArchiveUploadField(crate::view::ArchiveUploadField::Description),
        UiAction::AppendArchiveUploadCharacter('x'),
        UiAction::InsertArchiveUploadNewline,
        UiAction::DeleteArchiveUploadCharacter,
        UiAction::DeleteArchiveUploadWord,
        UiAction::ToggleArchiveUploadVideo,
        UiAction::SubmitArchiveUpload(generation),
        UiAction::DismissArchiveUpload,
    ] {
        controller.dispatch(action.clone());
        assert_eq!(
            controller.view.archive_upload_popup.as_ref(),
            Some(&before_review),
            "{action:?}"
        );
        assert_eq!(
            controller.view.archive_credentials_popup.as_ref(),
            Some(&before_credentials),
            "{action:?}"
        );
    }
    assert!(!directory.path().join("config.toml").exists());
    assert!(service.jobs.lock().unwrap().is_empty());
}

/// Every transition across credential entry invalidates all earlier publication tokens.
#[test]
fn archive_credentials_rotate_generation_on_entry_accept_and_cancel() {
    let (mut controller, service, _directory) = controller();
    controller.dispatch(UiAction::OpenArchiveUpload);
    let initial = controller
        .view
        .archive_upload_popup
        .as_ref()
        .unwrap()
        .generation;
    confirm(&mut controller);
    let entering = controller
        .view
        .archive_upload_popup
        .as_ref()
        .unwrap()
        .generation;
    assert_ne!(initial, entering);
    controller.dispatch(UiAction::DismissArchiveCredentials);
    let cancelled = controller
        .view
        .archive_upload_popup
        .as_ref()
        .unwrap()
        .generation;
    assert_ne!(entering, cancelled);
    confirm(&mut controller);
    let reopened = controller
        .view
        .archive_upload_popup
        .as_ref()
        .unwrap()
        .generation;
    assert_ne!(cancelled, reopened);
    controller.dispatch(UiAction::SubmitArchiveCredentials);
    assert!(
        controller
            .view
            .archive_credentials_popup
            .as_ref()
            .unwrap()
            .validation_error
            .is_some()
    );
    assert_eq!(
        controller
            .view
            .archive_upload_popup
            .as_ref()
            .unwrap()
            .generation,
        reopened
    );
    {
        let popup = controller.view.archive_credentials_popup.as_mut().unwrap();
        popup.access_key = "replacement-access".to_owned();
        popup.secret_key = "replacement-secret".to_owned();
    }
    controller.dispatch(UiAction::SubmitArchiveCredentials);
    let accepted = controller
        .view
        .archive_upload_popup
        .as_ref()
        .unwrap()
        .generation;
    assert_ne!(reopened, accepted);
    for generation in [initial, entering, cancelled, reopened] {
        controller.dispatch(UiAction::SubmitArchiveUpload(generation));
    }
    assert!(service.jobs.lock().unwrap().is_empty());
    assert_eq!(
        controller.view.archive_upload_popup.as_ref().unwrap().phase,
        ArchiveUploadPhase::Review
    );
    confirm(&mut controller);
    finish(&mut controller);
    assert_eq!(service.jobs.lock().unwrap().len(), 1);
}

/// Reopening cannot rediscover and reuse the same failed configured credentials.
#[test]
fn archive_failed_discovered_credentials_require_manual_replacement_until_accepted() {
    let (mut controller, service, _directory) = controller();
    service.discover_credentials.store(true, Ordering::Relaxed);
    service.reject_credentials.store(true, Ordering::Relaxed);
    controller.dispatch(UiAction::OpenArchiveUpload);
    confirm(&mut controller);
    finish(&mut controller);
    assert_eq!(service.discovery_count.load(Ordering::Relaxed), 1);
    controller.dispatch(UiAction::DismissArchiveUpload);
    controller.dispatch(UiAction::OpenArchiveUpload);
    confirm(&mut controller);
    assert!(controller.view.archive_credentials_popup.is_some());
    assert_eq!(service.discovery_count.load(Ordering::Relaxed), 1);
    controller.dispatch(UiAction::DismissArchiveCredentials);
    confirm(&mut controller);
    assert!(controller.view.archive_credentials_popup.is_some());
    assert_eq!(service.discovery_count.load(Ordering::Relaxed), 1);
    {
        let popup = controller.view.archive_credentials_popup.as_mut().unwrap();
        popup.access_key = "manual-access".to_owned();
        popup.secret_key = "manual-secret".to_owned();
    }
    service.reject_credentials.store(false, Ordering::Relaxed);
    controller.dispatch(UiAction::SubmitArchiveCredentials);
    assert_eq!(service.jobs.lock().unwrap().len(), 1);
    confirm(&mut controller);
    finish(&mut controller);
    assert_eq!(service.jobs.lock().unwrap().len(), 2);
    assert_eq!(service.discovery_count.load(Ordering::Relaxed), 1);
}

#[test]
fn archive_video_save_failure_does_not_change_the_review() {
    let (mut controller, service, directory) = controller();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "archive_upload = 42\n").unwrap();
    controller.dispatch(UiAction::OpenArchiveUpload);
    controller.dispatch(UiAction::ToggleArchiveUploadVideo);
    let popup = controller.view.archive_upload_popup.as_ref().unwrap();
    assert!(!popup.draft.upload_video);
    assert!(!controller.config.archive_upload.upload_video);
    assert!(popup.validation_error.is_some());
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "archive_upload = 42\n"
    );
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn archive_review_rejects_a_tampered_source_before_discovery_or_upload() {
    let (mut controller, service, _directory) = controller();
    controller.dispatch(UiAction::OpenArchiveUpload);
    controller
        .view
        .archive_upload_popup
        .as_mut()
        .unwrap()
        .draft
        .source_url = "https://www.youtube.com/watch?v=abcdefghijk".to_owned();
    confirm(&mut controller);
    assert!(controller.view.archive_credentials_popup.is_none());
    assert!(
        controller
            .view
            .archive_upload_popup
            .as_ref()
            .unwrap()
            .validation_error
            .is_some()
    );
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn archive_terminal_review_cannot_be_edited_or_resubmitted() {
    let (mut controller, service, _directory) = controller();
    controller.dispatch(UiAction::OpenArchiveUpload);
    for phase in [
        ArchiveUploadPhase::Failed,
        ArchiveUploadPhase::Cancelled,
        ArchiveUploadPhase::Complete,
    ] {
        controller.view.archive_upload_popup.as_mut().unwrap().phase = phase;
        let before = controller
            .view
            .archive_upload_popup
            .as_ref()
            .unwrap()
            .draft
            .clone();
        controller.dispatch(UiAction::ToggleArchiveUploadVideo);
        controller.dispatch(UiAction::AppendArchiveUploadCharacter('x'));
        confirm(&mut controller);
        assert_eq!(
            controller.view.archive_upload_popup.as_ref().unwrap().draft,
            before
        );
        assert!(service.jobs.lock().unwrap().is_empty());
    }
}

#[test]
fn archive_shutdown_cancels_the_worker_and_forgets_credentials() {
    let (mut controller, service, _directory) = controller();
    service.wait_for_cancel.store(true, Ordering::Relaxed);
    controller.archive_upload.credentials = Some(
        ArchiveUploadCredentials::new("test-access".to_owned(), "test-secret".to_owned()).unwrap(),
    );
    controller.dispatch(UiAction::OpenArchiveUpload);
    confirm(&mut controller);
    controller.shutdown_archive_upload();
    assert!(controller.archive_upload.credentials.is_none());
    assert!(controller.view.archive_credentials_popup.is_none());
    assert_eq!(service.jobs.lock().unwrap().len(), 1);
}

#[test]
fn archive_review_still_requires_a_title_before_prompting_for_keys() {
    let (mut controller, service, _directory) = controller();
    controller.dispatch(UiAction::OpenArchiveUpload);
    controller
        .view
        .archive_upload_popup
        .as_mut()
        .unwrap()
        .draft
        .title = "  ".to_owned();
    confirm(&mut controller);
    assert!(controller.view.archive_credentials_popup.is_none());
    assert!(
        controller
            .view
            .archive_upload_popup
            .as_ref()
            .unwrap()
            .validation_error
            .is_some()
    );
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn opening_archive_review_never_uploads_without_explicit_confirmation() {
    let (mut controller, service, _directory) = controller();
    controller.dispatch(UiAction::OpenArchiveUpload);
    let popup = controller
        .view
        .archive_upload_popup
        .as_ref()
        .expect("review popup");
    assert_eq!(popup.phase, ArchiveUploadPhase::Review);
    assert!(!popup.draft.upload_video);
    assert_eq!(
        popup.draft.source_url,
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
    );
    assert!(service.jobs.lock().unwrap().is_empty());
    confirm(&mut controller);
    assert!(controller.view.archive_credentials_popup.is_some());
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn archive_video_checkbox_remembers_on_reopen() {
    let (mut controller, service, directory) = controller();
    controller.dispatch(UiAction::OpenArchiveUpload);
    controller.dispatch(UiAction::ToggleArchiveUploadVideo);
    controller.dispatch(UiAction::DismissArchiveUpload);
    controller.dispatch(UiAction::OpenArchiveUpload);
    let popup = controller.view.archive_upload_popup.as_ref().unwrap();
    assert!(popup.draft.upload_video);
    assert!(
        Config::load_from_dir(directory.path())
            .unwrap()
            .archive_upload
            .upload_video
    );
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn archive_submit_snapshots_source_and_ignores_repeat_or_stale_confirmation() {
    let (mut controller, service, _directory) = controller();
    controller.archive_upload.credentials = Some(
        ArchiveUploadCredentials::new("test-access".to_owned(), "test-secret".to_owned()).unwrap(),
    );
    controller.dispatch(UiAction::OpenArchiveUpload);
    let stale = controller
        .view
        .archive_upload_popup
        .as_ref()
        .unwrap()
        .generation;
    controller.dispatch(UiAction::DismissArchiveUpload);
    controller.dispatch(UiAction::OpenArchiveUpload);
    controller.dispatch(UiAction::SubmitArchiveUpload(stale));
    assert!(service.jobs.lock().unwrap().is_empty());
    controller.youtube_results.clear();
    confirm(&mut controller);
    confirm(&mut controller);
    finish(&mut controller);
    assert_eq!(
        *service.jobs.lock().unwrap(),
        vec![("dQw4w9WgXcQ".to_owned(), false)]
    );
    let popup = controller.view.archive_upload_popup.as_ref().unwrap();
    assert_eq!(popup.phase, ArchiveUploadPhase::Complete);
    assert!(popup.result_url.is_some());
}

#[test]
fn archive_credentials_do_not_publish_and_are_not_saved() {
    let (mut controller, service, directory) = controller();
    controller.dispatch(UiAction::OpenArchiveUpload);
    confirm(&mut controller);
    let popup = controller.view.archive_credentials_popup.as_mut().unwrap();
    popup.access_key = "test-access".to_owned();
    popup.secret_key = "test-secret".to_owned();
    controller.dispatch(UiAction::SubmitArchiveCredentials);
    assert!(controller.view.archive_credentials_popup.is_none());
    assert_eq!(
        controller.view.archive_upload_popup.as_ref().unwrap().phase,
        ArchiveUploadPhase::Review
    );
    assert!(service.jobs.lock().unwrap().is_empty());
    assert!(!directory.path().join("secrets/archive-org.toml").exists());
}

#[test]
fn archive_cancel_stops_the_worker_without_a_success_link() {
    let (mut controller, service, _directory) = controller();
    service.wait_for_cancel.store(true, Ordering::Relaxed);
    controller.archive_upload.credentials = Some(
        ArchiveUploadCredentials::new("test-access".to_owned(), "test-secret".to_owned()).unwrap(),
    );
    controller.dispatch(UiAction::OpenArchiveUpload);
    confirm(&mut controller);
    controller.dispatch(UiAction::DismissArchiveUpload);
    finish(&mut controller);
    let popup = controller.view.archive_upload_popup.as_ref().unwrap();
    assert_eq!(popup.phase, ArchiveUploadPhase::Cancelled);
    assert!(popup.result_url.is_none());
}
