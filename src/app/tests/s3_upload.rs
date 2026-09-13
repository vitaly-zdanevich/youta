//! S3 review and worker tests using fake media and transport, never AWS.

use super::*;
use crate::app::s3_upload::{S3UploadJob, S3UploadService};
use crate::s3_upload::{S3UploadError, S3UploadResult};
use crate::view::{S3CredentialField, S3UploadPhase};

#[derive(Default)]
struct FakeS3Upload {
    jobs: Mutex<Vec<(String, String, bool)>>,
    credentials_required: AtomicBool,
    wait_for_cancel: AtomicBool,
}

impl S3UploadService for FakeS3Upload {
    fn run(
        &self,
        job: S3UploadJob,
        cancel: &Arc<AtomicBool>,
        progress: &mut dyn FnMut(S3UploadPhase, u64, Option<u64>),
    ) -> Result<S3UploadResult, S3UploadError> {
        if self.credentials_required.load(Ordering::Relaxed) && job.credentials.is_none() {
            return Err(S3UploadError::CredentialsRequired);
        }
        self.jobs.lock().unwrap().push((
            job.item.media.id.external_id,
            job.draft.object_key.clone(),
            job.draft.upload_video,
        ));
        progress(S3UploadPhase::Uploading, 4, Some(8));
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.wait_for_cancel.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        if cancel.load(Ordering::Relaxed) {
            Err(S3UploadError::Failed(
                "Cancelled; no object created".to_owned(),
            ))
        } else {
            Ok(S3UploadResult {
                location: format!("s3://{}/{}", job.draft.bucket, job.draft.object_key),
            })
        }
    }
}

fn controller() -> (AppController, Arc<FakeS3Upload>, tempfile::TempDir) {
    let directory = crate::test_support::canonical_tempdir("s3 upload controller");
    let mut config = Config::for_dir(directory.path());
    config.s3_upload.bucket = "test-audio".to_owned();
    config.s3_upload.region = "eu-central-1".to_owned();
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    controller.youtube_results = vec![SearchItem::Video(fixture_download_video())];
    controller.refresh_youtube_rows();
    let service = Arc::new(FakeS3Upload::default());
    controller.s3_upload.service = service.clone();
    (controller, service, directory)
}

fn generation(controller: &AppController) -> u64 {
    controller.view.s3_upload_popup.as_ref().unwrap().generation
}

fn confirm(controller: &mut AppController) {
    controller.dispatch(UiAction::SubmitS3Upload(generation(controller)));
}

fn finish(controller: &mut AppController) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while controller.s3_upload.worker_is_running() {
        controller.poll_s3_upload();
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn opening_and_editing_s3_review_never_uploads() {
    let (mut controller, service, _directory) = controller();
    controller.dispatch(UiAction::OpenS3Upload);
    assert_eq!(
        controller.view.s3_upload_popup.as_ref().unwrap().phase,
        S3UploadPhase::Review
    );
    controller.dispatch(UiAction::SelectS3UploadField(
        crate::view::S3UploadField::ObjectKey,
    ));
    controller.dispatch(UiAction::AppendS3UploadCharacter('x'));
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn s3_explicit_confirmation_uses_the_reviewed_source_and_only_runs_once() {
    let (mut controller, service, _directory) = controller();
    controller.dispatch(UiAction::OpenS3Upload);
    let key = controller
        .view
        .s3_upload_popup
        .as_ref()
        .unwrap()
        .draft
        .object_key
        .clone();
    let id = fixture_download_video().video_id;
    controller.youtube_results.clear();
    confirm(&mut controller);
    confirm(&mut controller);
    finish(&mut controller);
    assert_eq!(
        service.jobs.lock().unwrap().as_slice(),
        &[(id, key.clone(), false)]
    );
    assert_eq!(
        controller
            .view
            .s3_upload_popup
            .as_ref()
            .unwrap()
            .result_location
            .as_deref(),
        Some(format!("s3://test-audio/{key}").as_str())
    );
    confirm(&mut controller);
    assert_eq!(service.jobs.lock().unwrap().len(), 1);
}

#[test]
fn s3_missing_credentials_opens_editor_and_accepting_keys_returns_to_review() {
    let (mut controller, service, _directory) = controller();
    service.credentials_required.store(true, Ordering::Relaxed);
    controller.dispatch(UiAction::OpenS3Upload);
    let stale = generation(&controller);
    confirm(&mut controller);
    finish(&mut controller);
    assert!(controller.view.s3_credentials_popup.is_some());
    assert_ne!(generation(&controller), stale);
    controller.dispatch(UiAction::AppendS3CredentialCharacter('a'));
    controller.dispatch(UiAction::SelectS3CredentialField(
        S3CredentialField::SecretKey,
    ));
    controller.dispatch(UiAction::AppendS3CredentialCharacter('b'));
    controller.dispatch(UiAction::SubmitS3Credentials);
    assert!(controller.view.s3_credentials_popup.is_none());
    assert!(service.jobs.lock().unwrap().is_empty());
    controller.dispatch(UiAction::SubmitS3Upload(stale));
    assert!(service.jobs.lock().unwrap().is_empty());
    confirm(&mut controller);
    finish(&mut controller);
    assert_eq!(service.jobs.lock().unwrap().len(), 1);
}

#[test]
fn s3_credentials_are_modal_and_cancellation_invalidates_old_confirmation() {
    let (mut controller, service, directory) = controller();
    controller.dispatch(UiAction::OpenS3Upload);
    let old = generation(&controller);
    controller.dispatch(UiAction::OpenS3Credentials);
    let before = controller.view.s3_upload_popup.clone();
    for action in [
        UiAction::OpenS3Upload,
        UiAction::DismissS3Upload,
        UiAction::SubmitS3Upload(old),
        UiAction::ToggleS3UploadVideo,
        UiAction::AppendS3UploadCharacter('x'),
    ] {
        controller.dispatch(action);
        assert_eq!(controller.view.s3_upload_popup, before);
        assert!(controller.view.s3_credentials_popup.is_some());
    }
    assert!(!directory.path().join("config.toml").exists());
    controller.dispatch(UiAction::DismissS3Credentials);
    controller.dispatch(UiAction::SubmitS3Upload(old));
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn s3_cancel_retains_progress_until_worker_stops_and_does_not_report_success() {
    let (mut controller, service, _directory) = controller();
    service.wait_for_cancel.store(true, Ordering::Relaxed);
    controller.dispatch(UiAction::OpenS3Upload);
    confirm(&mut controller);
    controller.dispatch(UiAction::DismissS3Upload);
    assert_eq!(
        controller.view.s3_upload_popup.as_ref().unwrap().phase,
        S3UploadPhase::Cancelling
    );
    finish(&mut controller);
    assert_eq!(
        controller.view.s3_upload_popup.as_ref().unwrap().phase,
        S3UploadPhase::Cancelled
    );
    assert!(
        controller
            .view
            .s3_upload_popup
            .as_ref()
            .unwrap()
            .result_location
            .is_none()
    );
}

#[test]
fn s3_video_choice_is_remembered_but_never_authorizes_upload() {
    let (mut controller, service, directory) = controller();
    controller.dispatch(UiAction::OpenS3Upload);
    controller.dispatch(UiAction::ToggleS3UploadVideo);
    assert!(controller.config.s3_upload.upload_video);
    controller.dispatch(UiAction::DismissS3Upload);
    controller.dispatch(UiAction::OpenS3Upload);
    assert!(
        controller
            .view
            .s3_upload_popup
            .as_ref()
            .unwrap()
            .draft
            .upload_video
    );
    assert!(
        std::fs::read_to_string(directory.path().join("config.toml"))
            .unwrap()
            .contains("upload_video = true")
    );
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn s3_local_audio_is_exportable_without_enabling_video_or_running_helpers() {
    let (mut controller, service, directory) = controller();
    let path = directory.path().join("local.opus");
    std::fs::write(&path, b"mock Opus").unwrap();
    controller.view.screen = Screen::Local;
    controller.config.s3_upload.upload_video = true;
    controller.local_listing = Some(crate::local_browser::LocalDirectoryListing {
        path: directory.path().to_owned(),
        parent: None,
        entries: vec![crate::local_browser::LocalEntry {
            name: "local.opus".into(),
            path,
            kind: crate::local_browser::LocalEntryKind::Audio,
            size_bytes: Some(9),
            image_dimensions: None,
            directory_identity: None,
        }],
        truncated: false,
        inspected_entries: 1,
    });
    controller.refresh_selected_playlist_state();
    assert!(controller.view.s3_upload_available);
    controller.dispatch(UiAction::OpenS3Upload);
    let popup = controller.view.s3_upload_popup.as_ref().unwrap();
    assert!(!popup.video_available);
    assert!(!popup.draft.upload_video);
    controller.dispatch(UiAction::ToggleS3UploadVideo);
    assert!(
        !controller
            .view
            .s3_upload_popup
            .as_ref()
            .unwrap()
            .draft
            .upload_video
    );
    assert!(controller.config.s3_upload.upload_video);
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[cfg(feature = "bandcamp")]
#[test]
fn s3_supports_bandcamp_tracks_but_not_album_containers() {
    let (mut controller, service, _directory) = controller();
    controller.view.screen = Screen::Bandcamp;
    controller.bandcamp_results = vec![bandcamp_summary_fixture(
        "artist",
        "track",
        "Track",
        BandcampReleaseKind::Track,
    )];
    controller.refresh_bandcamp_rows();
    controller.refresh_selected_playlist_state();
    assert!(controller.view.s3_upload_available);
    controller.dispatch(UiAction::OpenS3Upload);
    assert!(
        !controller
            .view
            .s3_upload_popup
            .as_ref()
            .unwrap()
            .video_available
    );
    controller.dispatch(UiAction::DismissS3Upload);
    controller.bandcamp_results[0].kind = BandcampReleaseKind::Album;
    controller.refresh_selected_playlist_state();
    assert!(!controller.view.s3_upload_available);
    controller.dispatch(UiAction::OpenS3Upload);
    assert!(controller.view.s3_upload_popup.is_none());
    assert!(service.jobs.lock().unwrap().is_empty());
}

#[test]
fn s3_object_key_edits_survive_video_toggle() {
    let (mut controller, _service, _directory) = controller();
    controller.dispatch(UiAction::OpenS3Upload);
    controller
        .view
        .s3_upload_popup
        .as_mut()
        .unwrap()
        .draft
        .object_key = "exact/custom-name.bin".to_owned();
    controller.dispatch(UiAction::ToggleS3UploadVideo);
    assert_eq!(
        controller
            .view
            .s3_upload_popup
            .as_ref()
            .unwrap()
            .draft
            .object_key,
        "exact/custom-name.bin"
    );
}

/// A rendered confirmation must authorize the exact destination and format it displayed.
#[test]
fn s3_destination_or_video_edits_invalidate_rendered_confirmation() {
    for edit in [
        UiAction::AppendS3UploadCharacter('x'),
        UiAction::ToggleS3UploadVideo,
    ] {
        let (mut controller, service, _directory) = controller();
        controller.dispatch(UiAction::OpenS3Upload);
        controller.dispatch(UiAction::SelectS3UploadField(
            crate::view::S3UploadField::ObjectKey,
        ));
        let stale = generation(&controller);
        controller.dispatch(edit);
        controller.dispatch(UiAction::SubmitS3Upload(stale));
        assert!(
            !controller.s3_upload.worker_is_running(),
            "An old confirmation must not upload edited choices"
        );
        assert!(service.jobs.lock().unwrap().is_empty());
        assert_ne!(generation(&controller), stale);
        confirm(&mut controller);
        finish(&mut controller);
        assert_eq!(service.jobs.lock().unwrap().len(), 1);
    }
}
