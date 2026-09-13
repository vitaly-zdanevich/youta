//! One destination-reviewed, cancellable S3 upload with session-only credentials.

use super::*;
use crate::config::S3UploadConfig;
use crate::s3_upload::{
    S3UploadClient, S3UploadCredentials, S3UploadDraft, S3UploadError, S3UploadResult,
};
use crate::s3_upload_media::s3_media_capabilities;
use crate::view::{
    S3CredentialField, S3CredentialsPopupView, S3UploadField, S3UploadPhase, S3UploadPopupView,
};

/// Immutable source and destination moved off the UI thread after confirmation.
pub(super) struct S3UploadJob {
    pub(super) config: Config,
    pub(super) item: QueueItem,
    pub(super) draft: S3UploadDraft,
    pub(super) credentials: Option<S3UploadCredentials>,
}

/// Injectable worker boundary; tests never resolve real AWS credentials or media.
pub(super) trait S3UploadService: Send + Sync {
    fn run(
        &self,
        job: S3UploadJob,
        cancellation: &Arc<AtomicBool>,
        progress: &mut dyn FnMut(S3UploadPhase, u64, Option<u64>),
    ) -> Result<S3UploadResult, S3UploadError>;
}

struct SystemS3UploadService;

impl S3UploadService for SystemS3UploadService {
    fn run(
        &self,
        job: S3UploadJob,
        cancellation: &Arc<AtomicBool>,
        progress: &mut dyn FnMut(S3UploadPhase, u64, Option<u64>),
    ) -> Result<S3UploadResult, S3UploadError> {
        // Authentication happens first: missing keys must not trigger a media download.
        let session = S3UploadClient::new().resolve_credentials(
            &job.draft,
            job.credentials.as_ref(),
            cancellation,
        )?;
        let prepared = crate::s3_upload_media::prepare_s3_media(
            &job.config,
            &job.item.media,
            &job.item.playback_location,
            job.draft.upload_video,
            cancellation,
            |bytes, total| progress(S3UploadPhase::Preparing, bytes, total),
        )
        .map_err(S3UploadError::Failed)?;
        progress(S3UploadPhase::Uploading, 0, None);
        session.upload_file(prepared.path(), cancellation, |update| {
            progress(
                S3UploadPhase::Uploading,
                update.sent_bytes,
                Some(update.total_bytes),
            );
        })
    }
}

struct S3UploadWorker {
    generation: u64,
    cancellation: Arc<AtomicBool>,
    progress: Arc<Mutex<(S3UploadPhase, u64, Option<u64>)>>,
    started: Instant,
    thread: JoinHandle<Result<S3UploadResult, S3UploadError>>,
}

/// Owns the captured selection and keys separately from the serialized view.
pub(super) struct S3UploadState {
    pub(super) service: Arc<dyn S3UploadService>,
    credentials: Option<S3UploadCredentials>,
    selection: Option<QueueItem>,
    generation: u64,
    worker: Option<S3UploadWorker>,
}

impl Default for S3UploadState {
    fn default() -> Self {
        Self {
            service: Arc::new(SystemS3UploadService),
            credentials: None,
            selection: None,
            generation: 0,
            worker: None,
        }
    }
}

impl S3UploadState {
    #[cfg(test)]
    pub(super) fn worker_is_running(&self) -> bool {
        self.worker.is_some()
    }
}

impl AppController {
    /// Reuses exact queue sources, with stable saved locators for catalogue/history views.
    pub(super) fn selected_s3_queue_item(&self) -> Result<QueueItem, String> {
        if self.view.screen == Screen::Playlists
            && self
                .active_playlist
                .as_ref()
                .and_then(|playlist| playlist.entries.get(self.view.selected))
                .is_some_and(|entry| entry.segment.is_some())
        {
            return Err(
                "Select the complete media item to upload; saved segments are not exported"
                    .to_owned(),
            );
        }
        self.selected_export_queue_item().or_else(|_| {
            let media = self.selected_playlist_snapshot()?;
            queue_item_from_playlist_entry(&PlaylistEntry {
                media,
                segment: None,
                added_at: unix_time(),
            })
        })
    }

    /// Only the exact unobscured review may be edited or submitted.
    pub(super) fn s3_upload_review_is_editable(&self) -> bool {
        self.s3_upload.worker.is_none()
            && self.view.s3_credentials_popup.is_none()
            && self
                .view
                .s3_upload_popup
                .as_ref()
                .is_some_and(|popup| popup.phase == S3UploadPhase::Review)
    }

    /// Captures the selected finite source without resolving keys or sending media.
    pub(super) fn open_s3_upload(&mut self) {
        if self.s3_upload.worker.is_some() || self.view.s3_credentials_popup.is_some() {
            return;
        }
        let item = match self.selected_s3_queue_item() {
            Ok(item) => item,
            Err(error) => {
                self.view.status_line = error;
                return;
            }
        };
        let Some(capabilities) = s3_media_capabilities(&item.media, &item.playback_location) else {
            self.view.status_line =
                "Select a finite audio or video file to upload to S3".to_owned();
            return;
        };
        let upload_video = capabilities.upload_video && self.config.s3_upload.upload_video;
        let draft = S3UploadDraft {
            bucket: self.config.s3_upload.bucket.clone(),
            region: self.config.s3_upload.region.clone(),
            profile: self.config.s3_upload.profile.clone(),
            object_key: default_object_key(&item.media.title, upload_video),
            upload_video,
        };
        self.s3_upload.generation = self.s3_upload.generation.wrapping_add(1);
        self.s3_upload.selection = Some(item);
        self.view.s3_upload_popup = Some(S3UploadPopupView {
            generation: self.s3_upload.generation,
            draft,
            video_available: capabilities.upload_video,
            ..S3UploadPopupView::default()
        });
        self.view.status_line =
            "Review the S3 destination; nothing is uploaded until you confirm".to_owned();
    }

    fn rotate_s3_generation(&mut self) {
        self.s3_upload.generation = self.s3_upload.generation.wrapping_add(1);
        if let Some(popup) = &mut self.view.s3_upload_popup {
            popup.generation = self.s3_upload.generation;
        }
    }

    /// Entering the key editor invalidates earlier confirmations, including delayed GUI events.
    pub(super) fn open_s3_credentials(&mut self) {
        if !self.s3_upload_review_is_editable() {
            return;
        }
        self.rotate_s3_generation();
        self.view.s3_credentials_popup = Some(S3CredentialsPopupView::default());
    }

    /// Returning from credential entry is not authorization to upload.
    pub(super) fn dismiss_s3_credentials(&mut self) {
        if self.view.s3_credentials_popup.take().is_some() {
            self.rotate_s3_generation();
        }
    }

    /// Keeps the video preference across reviews, but never enables it for audio-only sources.
    pub(super) fn toggle_s3_upload_video(&mut self) {
        if !self.s3_upload_review_is_editable() {
            return;
        }
        let popup = self.view.s3_upload_popup.as_ref().unwrap();
        if !popup.video_available {
            return;
        }
        let video = !popup.draft.upload_video;
        let settings = S3UploadConfig {
            upload_video: video,
            ..self.config.s3_upload.clone()
        };
        match self.config.save_s3_upload_choices(settings) {
            Ok(()) => {
                let popup = self.view.s3_upload_popup.as_mut().unwrap();
                // Only the untouched suggested key changes; an edited key remains exact.
                if let Some(item) = &self.s3_upload.selection
                    && popup.draft.object_key == default_object_key(&item.media.title, !video)
                {
                    popup.draft.object_key = default_object_key(&item.media.title, video);
                }
                popup.draft.upload_video = video;
                popup.validation_error = None;
                self.rotate_s3_generation();
            }
            Err(error) => {
                self.view.s3_upload_popup.as_mut().unwrap().validation_error =
                    Some(error.to_string())
            }
        }
    }

    /// Edits destination text with protocol-sized limits; never edits the captured source.
    pub(super) fn edit_s3_upload(&mut self, character: Option<char>, word: bool) {
        if !self.s3_upload_review_is_editable() {
            return;
        }
        let popup = self.view.s3_upload_popup.as_mut().unwrap();
        let (field, limit) = match popup.selected_field {
            S3UploadField::Bucket => (&mut popup.draft.bucket, 63),
            S3UploadField::Region => (&mut popup.draft.region, 64),
            S3UploadField::ObjectKey => (&mut popup.draft.object_key, 1024),
            S3UploadField::Profile => (&mut popup.draft.profile, 128),
        };
        let before = field.clone();
        edit_field(field, character, word, limit);
        let changed = *field != before;
        popup.validation_error = None;
        if changed {
            // A new profile must not silently reuse another profile's manual credentials.
            if popup.selected_field == S3UploadField::Profile {
                self.s3_upload.credentials = None;
            }
            self.rotate_s3_generation();
        }
    }

    /// Validates one current review and starts a single bounded background operation.
    pub(super) fn submit_s3_upload(&mut self, generation: u64) {
        if !self.s3_upload_review_is_editable() {
            return;
        }
        let popup = self.view.s3_upload_popup.as_ref().unwrap();
        if generation != popup.generation || generation != self.s3_upload.generation {
            return;
        }
        let draft = popup.draft.clone();
        let Some(item) = self.s3_upload.selection.clone() else {
            return;
        };
        let validation = draft.validate().and_then(|()| {
            let capability = s3_media_capabilities(&item.media, &item.playback_location)
                .ok_or_else(|| "The selected upload source is no longer available".to_owned())?;
            if draft.upload_video && !capability.upload_video {
                Err("This source has no exportable video".to_owned())
            } else {
                Ok(())
            }
        });
        if let Err(error) = validation {
            self.view.s3_upload_popup.as_mut().unwrap().validation_error = Some(error);
            return;
        }
        let settings = S3UploadConfig {
            bucket: draft.bucket.clone(),
            region: draft.region.clone(),
            profile: draft.profile.clone(),
            // An audio-only selection must not reset the user's preference for video sources.
            upload_video: self.config.s3_upload.upload_video,
        };
        if settings != self.config.s3_upload
            && let Err(error) = self.config.save_s3_upload_choices(settings)
        {
            self.view.s3_upload_popup.as_mut().unwrap().validation_error = Some(error.to_string());
            return;
        }
        let job = S3UploadJob {
            config: self.config.clone(),
            item,
            draft,
            credentials: self.s3_upload.credentials.clone(),
        };
        let service = Arc::clone(&self.s3_upload.service);
        let cancellation = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new((S3UploadPhase::Preparing, 0, None)));
        let worker_cancel = Arc::clone(&cancellation);
        let worker_progress = Arc::clone(&progress);
        let thread = thread::Builder::new()
            .name("s3-upload".to_owned())
            .spawn(move || {
                service.run(job, &worker_cancel, &mut |phase, bytes, total| {
                    *worker_progress
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = (phase, bytes, total);
                })
            });
        match thread {
            Ok(thread) => {
                self.s3_upload.worker = Some(S3UploadWorker {
                    generation,
                    cancellation,
                    progress,
                    started: Instant::now(),
                    thread,
                });
                let popup = self.view.s3_upload_popup.as_mut().unwrap();
                popup.phase = S3UploadPhase::Preparing;
                popup.validation_error = None;
                self.view.status_line =
                    "Resolving AWS credentials and preparing the selected media".to_owned();
            }
            Err(_) => {
                self.view.s3_upload_popup.as_mut().unwrap().validation_error =
                    Some("Could not start the S3 upload worker".to_owned())
            }
        }
    }

    /// Copies only the latest progress sample, with no unbounded per-chunk event queue.
    pub(super) fn poll_s3_upload(&mut self) {
        let Some(worker) = self.s3_upload.worker.as_ref() else {
            return;
        };
        if let Some(popup) = self
            .view
            .s3_upload_popup
            .as_mut()
            .filter(|popup| popup.generation == worker.generation)
        {
            let (phase, bytes, total) = *worker
                .progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            popup.phase = if worker.cancellation.load(AtomicOrdering::Relaxed) {
                S3UploadPhase::Cancelling
            } else {
                phase
            };
            popup.uploaded_bytes = bytes;
            popup.total_bytes = total;
            popup.animation_frame =
                usize::try_from(worker.started.elapsed().as_millis() / 300).unwrap_or(0);
        }
        if !worker.thread.is_finished() {
            return;
        }
        let worker = self.s3_upload.worker.take().unwrap();
        let cancelled = worker.cancellation.load(AtomicOrdering::Relaxed);
        let result = worker.thread.join().unwrap_or_else(|_| {
            Err(S3UploadError::Failed(
                "S3 worker stopped unexpectedly; check the destination before retrying".to_owned(),
            ))
        });
        let Some(popup) = self
            .view
            .s3_upload_popup
            .as_mut()
            .filter(|popup| popup.generation == worker.generation)
        else {
            return;
        };
        match result {
            Ok(result) => {
                // Completion won the cancellation race: report the confirmed object truthfully.
                popup.phase = S3UploadPhase::Complete;
                popup.result_location = Some(result.location);
                popup.validation_error = None;
                self.view.status_line =
                    "S3 upload completed; existing bucket access rules still apply".to_owned();
            }
            Err(S3UploadError::CredentialsRequired) if !cancelled => {
                popup.phase = S3UploadPhase::Review;
                popup.validation_error = None;
                self.s3_upload.credentials = None;
                self.open_s3_credentials();
                self.view.status_line =
                    "Enter AWS session keys or return to choose a configured profile".to_owned();
            }
            Err(error) => {
                popup.phase = if cancelled {
                    S3UploadPhase::Cancelled
                } else {
                    S3UploadPhase::Failed
                };
                popup.validation_error = Some(error.to_string());
                self.view.status_line =
                    "S3 upload did not complete; review the result before retrying".to_owned();
            }
        }
    }

    /// Cancels the operation asynchronously; closing a credentials overlay cannot dismiss its parent.
    pub(super) fn dismiss_s3_upload(&mut self) {
        if self.view.s3_credentials_popup.is_some() {
            return;
        }
        if let Some(worker) = &self.s3_upload.worker {
            worker.cancellation.store(true, AtomicOrdering::Relaxed);
            if let Some(popup) = &mut self.view.s3_upload_popup {
                popup.phase = S3UploadPhase::Cancelling;
            }
        } else {
            self.view.s3_upload_popup = None;
            self.s3_upload.selection = None;
        }
    }

    /// Retains sensitive input in Rust only; the serialized editor exposes character counts.
    pub(super) fn edit_s3_credentials(&mut self, character: Option<char>, word: bool) {
        let Some(popup) = &mut self.view.s3_credentials_popup else {
            return;
        };
        let field = match popup.selected_field {
            S3CredentialField::AccessKey => &mut popup.access_key,
            S3CredentialField::SecretKey => &mut popup.secret_key,
            S3CredentialField::SessionToken => &mut popup.session_token,
        };
        edit_field(field, character, word, 16 * 1024);
        popup.validation_error = None;
    }

    /// Accepts keys for this session but requires a fresh explicit Upload action afterward.
    pub(super) fn submit_s3_credentials(&mut self) {
        let Some(popup) = self.view.s3_credentials_popup.as_ref() else {
            return;
        };
        match S3UploadCredentials::new(
            popup.access_key.clone(),
            popup.secret_key.clone(),
            (!popup.session_token.is_empty()).then(|| popup.session_token.clone()),
        ) {
            Ok(credentials) => {
                self.s3_upload.credentials = Some(credentials);
                self.dismiss_s3_credentials();
            }
            Err(error) => {
                self.view
                    .s3_credentials_popup
                    .as_mut()
                    .unwrap()
                    .validation_error = Some(error)
            }
        }
    }

    /// Stops staging and bounded SDK operations before forgetting session credentials.
    pub(super) fn shutdown_s3_upload(&mut self) {
        if let Some(worker) = self.s3_upload.worker.take() {
            worker.cancellation.store(true, AtomicOrdering::Relaxed);
            let _ = worker.thread.join();
        }
        self.s3_upload.credentials = None;
        self.view.s3_credentials_popup = None;
    }
}

/// Generates a readable suggestion without allowing a title to inject path components.
fn default_object_key(title: &str, video: bool) -> String {
    let title: String = title
        .chars()
        .filter(|character| !character.is_control())
        .map(|character| {
            if matches!(character, '/' | '\\') {
                '_'
            } else {
                character
            }
        })
        .take(180)
        .collect();
    let stem = title.trim().trim_matches('.');
    let stem = if stem.is_empty() { "media" } else { stem };
    format!("youta/{stem}.{}", if video { "mkv" } else { "opus" })
}

/// Bounded single-line editing shared only by this feature's review and key editor.
fn edit_field(field: &mut String, character: Option<char>, word: bool, limit: usize) {
    if let Some(character) = character {
        if !character.is_control() && field.len().saturating_add(character.len_utf8()) <= limit {
            field.push(character);
        }
    } else if word {
        while field.ends_with(char::is_whitespace) {
            field.pop();
        }
        while field
            .chars()
            .last()
            .is_some_and(|character| !character.is_whitespace())
        {
            field.pop();
        }
    } else {
        field.pop();
    }
}
