//! One review-owned Internet Archive transfer, with bounded worker progress.

use super::*;
use crate::archive_upload::{
    ArchiveUploadClient, ArchiveUploadCredentials, ArchiveUploadDraft, ArchiveUploadResult,
    ArchiveUploadSource, discover_archive_upload_credentials,
};
use crate::view::{
    ArchiveCredentialsPopupView, ArchiveUploadField, ArchiveUploadPhase, ArchiveUploadPopupView,
};

const CREDENTIALS_GUIDE: &str = "https://archive.org/account/s3.php";

/// Immutable request moved to the worker only after explicit upload confirmation.
pub(super) struct ArchiveUploadJob {
    pub(super) config: Config,
    pub(super) media: MediaItem,
    pub(super) playback_location: String,
    pub(super) draft: ArchiveUploadDraft,
    pub(super) credentials: ArchiveUploadCredentials,
}

/// Injectable side-effect boundary; controller tests never fetch credentials or upload.
pub(super) trait ArchiveUploadService: Send + Sync {
    fn discover(&self, config: &Config) -> Result<Option<ArchiveUploadCredentials>, String>;
    fn run(
        &self,
        job: ArchiveUploadJob,
        cancellation: &Arc<AtomicBool>,
        progress: &mut dyn FnMut(ArchiveUploadPhase, u64, Option<u64>),
    ) -> Result<ArchiveUploadResult, String>;
}

struct SystemArchiveUploadService;

impl ArchiveUploadService for SystemArchiveUploadService {
    fn discover(&self, config: &Config) -> Result<Option<ArchiveUploadCredentials>, String> {
        discover_archive_upload_credentials(config.config_dir())
    }

    fn run(
        &self,
        job: ArchiveUploadJob,
        cancellation: &Arc<AtomicBool>,
        progress: &mut dyn FnMut(ArchiveUploadPhase, u64, Option<u64>),
    ) -> Result<ArchiveUploadResult, String> {
        let prepared = if job.media.id.source == SourceKind::Local {
            crate::archive_upload_media::prepare_local_archive_media(
                &job.config,
                &job.media,
                &job.playback_location,
                job.draft.upload_video,
                cancellation,
                |bytes, total| progress(ArchiveUploadPhase::Preparing, bytes, total),
            )?
        } else {
            crate::archive_upload_media::prepare_archive_media(
                &job.config,
                &job.media,
                job.draft.upload_video,
                cancellation,
                |bytes, total| progress(ArchiveUploadPhase::Preparing, bytes, total),
            )?
        };
        if cancellation.load(AtomicOrdering::Relaxed) {
            return Err("Archive upload cancelled before publication".to_owned());
        }
        progress(ArchiveUploadPhase::Uploading, 0, None);
        ArchiveUploadClient::default().upload_file(
            &job.credentials,
            &job.draft,
            prepared.path(),
            prepared.filename(),
            cancellation,
            |update| {
                progress(
                    ArchiveUploadPhase::Uploading,
                    update.sent_bytes,
                    Some(update.total_bytes),
                )
            },
        )
    }
}

struct ArchiveUploadWorker {
    generation: u64,
    cancellation: Arc<AtomicBool>,
    progress: Arc<Mutex<(ArchiveUploadPhase, u64, Option<u64>)>>,
    started: Instant,
    /// Final result is published before notification, independently of thread teardown.
    response: Receiver<Result<ArchiveUploadResult, String>>,
    thread: JoinHandle<()>,
}

#[cfg(test)]
mod wake_tests {
    use super::*;

    /// The final queue entry retires ownership before thread cleanup can finish.
    #[test]
    fn archive_upload_result_does_not_wait_for_thread_exit() {
        let directory = crate::test_support::canonical_tempdir("archive upload wake");
        let mut controller = AppController::new(
            Config::for_dir(directory.path()),
            StateStore::open_in_memory().unwrap(),
            None,
            None,
        );
        for disconnected in [false, true] {
            let (sender, response) = bounded(1);
            let (published, publication) = bounded(1);
            let (release, released) = bounded(1);
            let thread = thread::spawn(move || {
                if !disconnected {
                    let _ = sender.send(Err("fixture failure".to_owned()));
                }
                drop(sender);
                published.send(()).unwrap();
                let _ = released.recv_timeout(Duration::from_secs(5));
            });
            publication.recv_timeout(Duration::from_secs(5)).unwrap();
            controller.archive_upload.worker = Some(ArchiveUploadWorker {
                generation: 1,
                cancellation: Arc::new(AtomicBool::new(false)),
                progress: Arc::new(Mutex::new((ArchiveUploadPhase::Preparing, 0, None))),
                started: Instant::now(),
                response,
                thread,
            });
            controller.poll_archive_upload();
            let retired = controller.archive_upload.worker.is_none();
            release.send(()).unwrap();
            assert!(
                retired,
                "published results and disconnects must not wait for thread exit"
            );
        }
    }
}

/// Secrets and source ownership stay outside the serializable view model.
pub(super) struct ArchiveUploadState {
    pub(super) service: Arc<dyn ArchiveUploadService>,
    pub(super) credentials: Option<ArchiveUploadCredentials>,
    /// Failed publication requires explicit replacement rather than rediscovering the same keys.
    require_manual_credentials: bool,
    selection: Option<QueueItem>,
    generation: u64,
    worker: Option<ArchiveUploadWorker>,
}

impl Default for ArchiveUploadState {
    fn default() -> Self {
        Self {
            service: Arc::new(SystemArchiveUploadService),
            credentials: None,
            require_manual_credentials: false,
            selection: None,
            generation: 0,
            worker: None,
        }
    }
}

impl AppController {
    /// Checks the current row without spawning media probes during a UI refresh.
    pub(super) fn archive_upload_is_available(&self) -> bool {
        if self.view.screen == Screen::Local {
            return self
                .local_entry_index()
                .and_then(|index| self.local_listing.as_ref()?.entries.get(index))
                .is_some_and(|entry| entry.kind.is_playable() && entry.path.is_file());
        }
        self.selected_queue_item()
            .is_ok_and(|item| archive_upload_video_available(&item).is_some())
    }

    /// Only a fresh, unobscured review may accept metadata or upload-option changes.
    pub(super) fn archive_upload_review_is_editable(&self) -> bool {
        self.archive_upload.worker.is_none()
            && self.view.archive_credentials_popup.is_none()
            && self
                .view
                .archive_upload_popup
                .as_ref()
                .is_some_and(|popup| popup.phase.is_editable())
    }

    /// Invalidates confirmations rendered before a credential-editor boundary.
    fn rotate_archive_upload_generation(&mut self) {
        self.archive_upload.generation = self.archive_upload.generation.wrapping_add(1);
        if let Some(popup) = &mut self.view.archive_upload_popup {
            popup.generation = self.archive_upload.generation;
        }
    }

    /// Returns to the same review without authorizing publication or retaining old tokens.
    pub(super) fn dismiss_archive_credentials(&mut self) {
        if self.view.archive_credentials_popup.take().is_some() {
            self.rotate_archive_upload_generation();
        }
    }

    /// Captures one YouTube or local item before an editable, non-publishing review.
    pub(super) fn open_archive_upload(&mut self) {
        if self.archive_upload.worker.is_some() || self.view.archive_credentials_popup.is_some() {
            return;
        }
        let mut item = match self.selected_export_queue_item() {
            Ok(item) if archive_upload_video_available(&item).is_some() => item,
            _ => {
                self.view.status_line =
                    "Select a YouTube video or local media file to upload to archive.org"
                        .to_owned();
                return;
            }
        };
        let video_available = archive_upload_video_available(&item).unwrap_or(false);
        let media = &mut item.media;
        if media.id.source == SourceKind::YouTube
            && let Some(details) = self
                .view
                .details
                .as_ref()
                .filter(|details| details.media_id.as_ref() == Some(&media.id))
        {
            if !details.description.is_empty() {
                media.description = Some(details.description.clone());
            }
            if !details.channel_name.is_empty() {
                media.creator = Some(details.channel_name.clone());
            }
        }
        // Local details contain filesystem paths, which are never public metadata.
        let draft = if media.id.source == SourceKind::Local {
            ArchiveUploadDraft::new_local(media.title.clone(), String::new(), media.creator.clone())
        } else {
            ArchiveUploadDraft::new(
                media.webpage_url.clone(),
                media.title.clone(),
                media.description.clone().unwrap_or_default(),
                media.creator.clone(),
            )
        };
        let mut draft = match draft {
            Ok(draft) => draft,
            Err(error) => {
                self.view.status_line = error;
                return;
            }
        };
        draft.upload_video = video_available && self.config.archive_upload.upload_video;
        self.archive_upload.generation = self.archive_upload.generation.wrapping_add(1);
        self.archive_upload.selection = Some(item);
        self.view.archive_credentials_popup = None;
        self.view.archive_upload_popup = Some(ArchiveUploadPopupView {
            generation: self.archive_upload.generation,
            draft,
            video_available,
            ..ArchiveUploadPopupView::default()
        });
        self.view.status_line =
            "Review the public upload; nothing is published until you confirm".to_owned();
    }

    /// Remembers only the video choice, never an instruction to start uploading.
    pub(super) fn toggle_archive_upload_video(&mut self) {
        if !self.archive_upload_review_is_editable() {
            return;
        }
        let Some(popup) = self
            .view
            .archive_upload_popup
            .as_ref()
            .filter(|popup| popup.phase == ArchiveUploadPhase::Review && popup.video_available)
        else {
            return;
        };
        let value = !popup.draft.upload_video;
        match self.config.save_archive_upload_video(value) {
            Ok(()) => {
                if let Some(popup) = self.view.archive_upload_popup.as_mut() {
                    popup.draft.upload_video = value;
                    popup.validation_error = None;
                }
            }
            Err(error) => {
                if let Some(popup) = self.view.archive_upload_popup.as_mut() {
                    popup.validation_error = Some(error.to_string());
                }
            }
        }
    }

    /// Edits bounded plain-text metadata; the canonical source URL is read-only.
    pub(super) fn edit_archive_upload(&mut self, text: Option<char>, delete_word: bool) {
        if !self.archive_upload_review_is_editable() {
            return;
        }
        let Some(popup) = self
            .view
            .archive_upload_popup
            .as_mut()
            .filter(|popup| popup.phase == ArchiveUploadPhase::Review)
        else {
            return;
        };
        let (field, limit) = match popup.selected_field {
            ArchiveUploadField::Identifier => (&mut popup.draft.identifier, 100),
            ArchiveUploadField::Title => (&mut popup.draft.title, 1024),
            ArchiveUploadField::Description => (&mut popup.draft.description, 64 * 1024),
            ArchiveUploadField::Creator => (&mut popup.draft.creator, 1024),
        };
        if let Some(character) = text {
            if (!character.is_control()
                || character == '\n' && popup.selected_field == ArchiveUploadField::Description)
                && field.len().saturating_add(character.len_utf8()) <= limit
            {
                field.push(character);
            }
        } else if delete_word {
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
        popup.validation_error = None;
    }

    /// Requires valid metadata and the exact confirmed review before starting one worker.
    pub(super) fn submit_archive_upload(&mut self, generation: u64) {
        if self.archive_upload.worker.is_some() || self.view.archive_credentials_popup.is_some() {
            return;
        }
        let Some(popup) = self.view.archive_upload_popup.as_ref().filter(|popup| {
            popup.phase == ArchiveUploadPhase::Review
                && popup.generation == generation
                && generation == self.archive_upload.generation
        }) else {
            return;
        };
        let draft = popup.draft.clone();
        let Some(item) = self.archive_upload.selection.clone() else {
            return;
        };
        let validation = draft.validate().and_then(|()| {
            let source_matches = match (&item.media.id.source, draft.source) {
                (SourceKind::YouTube, ArchiveUploadSource::YouTube) => {
                    draft.source_url == item.media.webpage_url.as_str()
                }
                (SourceKind::Local, ArchiveUploadSource::Local) => draft.source_url.is_empty(),
                _ => false,
            };
            let format_matches = archive_upload_video_available(&item)
                .is_some_and(|video_available| !draft.upload_video || video_available);
            if source_matches && format_matches {
                Ok(())
            } else {
                Err("The upload source changed; close and reopen the review".to_owned())
            }
        });
        if let Err(error) = validation {
            self.view
                .archive_upload_popup
                .as_mut()
                .unwrap()
                .validation_error = Some(error);
            return;
        }
        if self.archive_upload.credentials.is_none()
            && !self.archive_upload.require_manual_credentials
        {
            match self.archive_upload.service.discover(&self.config) {
                Ok(credentials) => self.archive_upload.credentials = credentials,
                Err(error) => {
                    self.view
                        .archive_upload_popup
                        .as_mut()
                        .unwrap()
                        .validation_error = Some(error);
                }
            }
        }
        let Some(credentials) = self.archive_upload.credentials.clone() else {
            self.rotate_archive_upload_generation();
            self.view.archive_credentials_popup = Some(ArchiveCredentialsPopupView {
                access_key: String::new(),
                secret_key: String::new(),
                secret_selected: false,
                validation_error: None,
            });
            return;
        };
        let job = ArchiveUploadJob {
            config: self.config.clone(),
            media: item.media,
            playback_location: item.playback_location,
            draft,
            credentials,
        };
        let service = Arc::clone(&self.archive_upload.service);
        let cancellation = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new((ArchiveUploadPhase::Preparing, 0, None)));
        let worker_cancel = Arc::clone(&cancellation);
        let worker_progress = Arc::clone(&progress);
        let (sender, response) = bounded(1);
        let sender = ResponseSender::new(sender, self.worker_notifier.clone());
        let notifier = self.worker_notifier.clone();
        let thread = thread::Builder::new()
            .name("archive-upload".to_owned())
            .spawn(move || {
                let mut last_notification = Instant::now() - Duration::from_millis(100);
                let mut last_phase = ArchiveUploadPhase::Preparing;
                let result = service.run(job, &worker_cancel, &mut |phase, bytes, total| {
                    *worker_progress
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = (phase, bytes, total);
                    if phase != last_phase
                        || last_notification.elapsed() >= Duration::from_millis(100)
                    {
                        last_phase = phase;
                        last_notification = Instant::now();
                        notifier.wake();
                    }
                });
                let _ = sender.send(result);
            });
        match thread {
            Ok(thread) => {
                self.archive_upload.worker = Some(ArchiveUploadWorker {
                    generation,
                    cancellation,
                    progress,
                    started: Instant::now(),
                    response,
                    thread,
                });
                let popup = self.view.archive_upload_popup.as_mut().unwrap();
                popup.phase = ArchiveUploadPhase::Preparing;
                popup.validation_error = None;
                self.view.status_line = "Preparing media for archive.org".to_owned();
            }
            Err(_) => {
                self.view
                    .archive_upload_popup
                    .as_mut()
                    .unwrap()
                    .validation_error = Some("Could not start the Archive upload worker".to_owned())
            }
        }
    }

    /// Updates a single progress snapshot, never building an unbounded message queue.
    pub(super) fn poll_archive_upload(&mut self) {
        let Some(worker) = self.archive_upload.worker.as_ref() else {
            return;
        };
        if let Some(popup) = self
            .view
            .archive_upload_popup
            .as_mut()
            .filter(|popup| popup.generation == worker.generation)
        {
            let (phase, bytes, total) = *worker
                .progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            popup.phase = if worker.cancellation.load(AtomicOrdering::Relaxed) {
                ArchiveUploadPhase::Cancelling
            } else {
                phase
            };
            popup.uploaded_bytes = bytes;
            popup.total_bytes = total;
            popup.animation_frame =
                usize::try_from(worker.started.elapsed().as_millis() / 300).unwrap_or(0);
        }
        let result = match worker.response.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(
                "Archive upload worker stopped unexpectedly; check the item before trying again"
                    .to_owned(),
            ),
        };
        let worker = self.archive_upload.worker.take().unwrap();
        reap_published_worker(worker.thread);
        let Some(popup) = self
            .view
            .archive_upload_popup
            .as_mut()
            .filter(|popup| popup.generation == worker.generation)
        else {
            return;
        };
        match result {
            Ok(result) => {
                popup.phase = ArchiveUploadPhase::Complete;
                popup.result_url = Some(result.item_url);
                popup.validation_error = None;
                self.view.status_line =
                    "Upload accepted by archive.org; processing may continue".to_owned();
            }
            Err(error) => {
                popup.phase = if worker.cancellation.load(AtomicOrdering::Relaxed) {
                    ArchiveUploadPhase::Cancelled
                } else {
                    // The backend deliberately exposes redacted strings, not retryable error kinds.
                    // Conservatively require manual keys after any failed publication attempt.
                    self.archive_upload.credentials = None;
                    self.archive_upload.require_manual_credentials = true;
                    ArchiveUploadPhase::Failed
                };
                popup.validation_error = Some(error);
                self.view.status_line =
                    "Archive upload did not complete; review the result before retrying".to_owned();
            }
        }
    }

    /// Cancels in flight without blocking the UI or treating a partial publication as success.
    pub(super) fn dismiss_archive_upload(&mut self) {
        if self.view.archive_credentials_popup.is_some() {
            return;
        }
        if let Some(worker) = &self.archive_upload.worker {
            worker.cancellation.store(true, AtomicOrdering::Relaxed);
            if let Some(popup) = &mut self.view.archive_upload_popup {
                popup.phase = ArchiveUploadPhase::Cancelling;
            }
            return;
        }
        self.view.archive_upload_popup = None;
        self.view.archive_credentials_popup = None;
        self.archive_upload.selection = None;
    }

    /// Edits keys only in the in-memory credential view, which serializes lengths alone.
    pub(super) fn edit_archive_credentials(&mut self, text: Option<char>, delete_word: bool) {
        let Some(popup) = &mut self.view.archive_credentials_popup else {
            return;
        };
        let field = if popup.secret_selected {
            &mut popup.secret_key
        } else {
            &mut popup.access_key
        };
        if let Some(character) = text {
            if !character.is_control() && field.len().saturating_add(character.len_utf8()) <= 1024 {
                field.push(character);
            }
        } else if delete_word {
            field.clear();
        } else {
            field.pop();
        }
        popup.validation_error = None;
    }

    /// Accepts session-only credentials and returns to review, never publishing on key entry.
    pub(super) fn submit_archive_credentials(&mut self) {
        let Some(popup) = self.view.archive_credentials_popup.as_ref() else {
            return;
        };
        match ArchiveUploadCredentials::new(popup.access_key.clone(), popup.secret_key.clone()) {
            Ok(credentials) => {
                self.archive_upload.credentials = Some(credentials);
                self.archive_upload.require_manual_credentials = false;
                self.dismiss_archive_credentials();
                if let Some(popup) = &mut self.view.archive_upload_popup {
                    popup.validation_error = None;
                }
            }
            Err(error) => {
                self.view
                    .archive_credentials_popup
                    .as_mut()
                    .unwrap()
                    .validation_error = Some(error)
            }
        }
    }

    /// Opens only a canonical public result page returned for this review.
    pub(super) fn open_archive_upload_result(&mut self) {
        if let Some(url) = self
            .view
            .archive_upload_popup
            .as_ref()
            .and_then(|popup| popup.result_url.clone())
            && url.scheme() == "https"
            && url.host_str() == Some("archive.org")
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().starts_with("/details/")
        {
            self.open_external_url(url.as_str());
        }
    }

    pub(super) fn open_archive_credentials_guide(&mut self) {
        self.open_external_url(CREDENTIALS_GUIDE);
    }

    /// Stops media helpers and waits only for bounded upload I/O during application shutdown.
    pub(super) fn shutdown_archive_upload(&mut self) {
        if let Some(worker) = self.archive_upload.worker.take() {
            worker.cancellation.store(true, AtomicOrdering::Relaxed);
            let _ = worker.thread.join();
        }
        self.view.archive_credentials_popup = None;
        self.archive_upload.credentials = None;
    }
}

/// Returns finite source capabilities without resolving or disclosing local paths.
fn archive_upload_video_available(item: &QueueItem) -> Option<bool> {
    match item.media.id.source {
        SourceKind::YouTube if matches!(item.media.kind, MediaKind::Audio | MediaKind::Video) => {
            Some(true)
        }
        SourceKind::Local
            if matches!(item.media.kind, MediaKind::Audio | MediaKind::Video)
                && item.media.webpage_url.scheme() == "file"
                && item.playback_location == item.media.webpage_url.as_str()
                && item
                    .media
                    .webpage_url
                    .to_file_path()
                    .is_ok_and(|path| path.is_file()) =>
        {
            Some(item.media.kind == MediaKind::Video)
        }
        _ => None,
    }
}
