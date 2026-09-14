//! Durable manual download scheduling, independent of playback and subscription checks.
//!
//! Only stable source identities and confirmed format choices cross the persistence
//! boundary. Workers remain serialized and completion belongs to one numbered attempt.

use super::*;
use crate::download_queue::{
    DownloadQueue, DownloadQueueEntry, DownloadQueueState, DownloadSource, QueuedDownloadFormat,
};
use crate::view::{DownloadQueueEntryView, DownloadQueuePopupView};

/// Owns manual intent and transient UI marks without owning provider credentials.
#[derive(Default)]
pub(super) struct ManualDownloads {
    pub(super) queue: DownloadQueue,
    pub(super) marks: Vec<DownloadSource>,
    pub(super) active: Option<(u64, u64)>,
    choosing: Option<u64>,
    pub(super) yandex_generation: Option<u64>,
    pub(super) blocked: bool,
    pending_write: Option<DownloadQueue>,
    downloaded: HashSet<MediaId>,
    marker_check: Option<Instant>,
    /// A completed Archive cache waiting for the preceding copy worker to retire.
    #[cfg(all(feature = "yt-dlp", feature = "backend-mpv"))]
    deferred_cache: Option<(QueueItem, DownloadRequest)>,
}

impl AppController {
    /// Restores pending work; interrupted transfers resolve fresh URLs on their next attempt.
    pub(super) fn restore_manual_downloads(&mut self) {
        match self.store.download_queue() {
            Ok(mut queue) => {
                let recovered = queue.recover_running();
                if recovered && !self.save_manual_download_queue(queue.clone()) {
                    return;
                }
                self.manual_downloads.queue = queue;
                self.refresh_download_markers(true);
            }
            Err(error) => {
                self.manual_downloads.blocked = true;
                self.show_error("Could not restore the download queue", &error);
            }
        }
    }

    /// Commits a complete transition before any new worker is allowed to start.
    fn save_manual_download_queue(&mut self, queue: DownloadQueue) -> bool {
        match self.store.save_download_queue(&queue) {
            Ok(()) => {
                self.manual_downloads.queue = queue;
                self.manual_downloads.pending_write = None;
                self.refresh_download_queue_popup();
                true
            }
            Err(error) => {
                self.manual_downloads.blocked = true;
                self.manual_downloads.pending_write = Some(queue);
                self.show_error("Could not save the download queue", &error);
                false
            }
        }
    }

    /// Captures a selected source; containers and already-local files are not downloads.
    pub(super) fn selected_manual_download_source(&self) -> Result<DownloadSource, String> {
        if matches!(self.view.screen, Screen::Local | Screen::Downloaded) {
            return Err("This file is already local; select a remote media item".to_owned());
        }
        let item = self.selected_queue_item().or_else(|_| {
            let snapshot = self.selected_playlist_snapshot()?;
            Ok::<_, String>(QueueItem {
                media: MediaItem {
                    id: snapshot.id,
                    kind: snapshot.kind,
                    title: snapshot.title,
                    creator: snapshot.creator,
                    description: snapshot.description,
                    webpage_url: snapshot.webpage_url,
                    thumbnail_url: snapshot.thumbnail_url,
                    duration_seconds: snapshot.duration_seconds,
                    published_at: None,
                    statistics: MediaStatistics::default(),
                    license: MediaLicense::Unknown,
                    chapters: Vec::new(),
                    captions: Vec::new(),
                },
                playback_location: snapshot.replay_locator,
                start_at_seconds: None,
                added_at: unix_time(),
            })
        })?;
        if item.media.id.source == SourceKind::Local {
            return Err("This file is already local; select a remote media item".to_owned());
        }
        if item.media.id.source == SourceKind::YandexMusic {
            if !cfg!(feature = "yandex-music") {
                return Err("This build omits Yandex Music downloads".to_owned());
            }
        } else if !cfg!(feature = "yt-dlp") {
            return Err(
                "Download support was disabled when this Youta binary was built".to_owned(),
            );
        }
        let webpage_url = if item.media.id.source == SourceKind::YouTube {
            // Invidious may advertise its own watch page. Replay always uses
            // the canonical ID, never that provider instance or a signed CDN.
            url::Url::parse(&youtube_video_url(&item.media.id.external_id))
                .map_err(|_| "Invalid YouTube video identity".to_owned())?
        } else {
            item.media.webpage_url.clone()
        };
        let download_url = if matches!(
            item.media.id.source,
            SourceKind::Rss | SourceKind::ApplePodcasts | SourceKind::Radio | SourceKind::LibriVox
        ) {
            url::Url::parse(&item.playback_location)
                .map_err(|_| "No podcast enclosure is available".to_owned())?
        } else {
            webpage_url.clone()
        };
        let source = DownloadSource {
            media_id: item.media.id,
            kind: item.media.kind,
            title: item.media.title,
            creator: item.media.creator,
            webpage_url,
            download_url,
            duration_seconds: item.media.duration_seconds,
        };
        // Reuse the durable boundary even for temporary marks, so enqueue cannot
        // unexpectedly retain credentials after the user has marked a whole page.
        let mut validation = DownloadQueue::default();
        let id = validation.next_id;
        validation.next_id = id
            .checked_add(1)
            .ok_or("Download queue IDs are exhausted")?;
        validation.entries.push(new_entry(id, source.clone()));
        validation.validate()?;
        Ok(source)
    }

    /// Toggles a stable captured identity, without moving the playback cursor.
    pub(super) fn toggle_download_mark(&mut self) {
        match self.selected_manual_download_source() {
            Ok(source) => {
                if let Some(index) = self
                    .manual_downloads
                    .marks
                    .iter()
                    .position(|marked| marked.media_id == source.media_id)
                {
                    self.manual_downloads.marks.remove(index);
                } else {
                    self.manual_downloads.marks.push(source);
                }
                self.view.status_line = format!(
                    "{} marked for download · [d] Enqueue · [Ctrl+D] Queue",
                    self.manual_downloads.marks.len()
                );
                self.refresh_download_markers(false);
            }
            Err(error) => self.view.status_line = error,
        }
    }

    /// Enqueues the marked batch, or the current item when no marks are present.
    pub(super) fn enqueue_selected_manual_download(&mut self) {
        if self.manual_downloads.blocked {
            self.view.status_line =
                "The download queue could not be saved; restart after resolving the state error"
                    .to_owned();
            return;
        }
        let sources = if self.manual_downloads.marks.is_empty() {
            match self.selected_manual_download_source() {
                Ok(source) => vec![source],
                Err(error) => {
                    self.view.status_line = error;
                    return;
                }
            }
        } else {
            self.manual_downloads.marks.clone()
        };
        let mut queue = self.manual_downloads.queue.clone();
        let mut added = 0;
        for source in sources {
            if queue.entries.iter().any(|entry| {
                entry.source.media_id == source.media_id
                    && matches!(
                        entry.state,
                        DownloadQueueState::Queued | DownloadQueueState::Running
                    )
            }) {
                continue;
            }
            let id = queue.next_id;
            let Some(next_id) = id.checked_add(1) else {
                self.view.status_line = "Download queue IDs are exhausted".to_owned();
                return;
            };
            queue.next_id = next_id;
            queue.entries.push(new_entry(id, source));
            added += 1;
        }
        if added == 0 {
            self.manual_downloads.marks.clear();
            self.view.status_line =
                "The selected items are already queued or downloading · [Ctrl+D] Queue".to_owned();
            self.refresh_download_markers(false);
            return;
        }
        if !self.save_manual_download_queue(queue) {
            return;
        }
        self.manual_downloads.marks.clear();
        self.refresh_download_markers(false);
        self.view.status_line = format!("Queued {added} item(s) · [Ctrl+D] Download queue");
        self.poll_manual_download_queue();
    }

    /// Runs at most one queued job and leaves format questions attached to their source.
    pub(super) fn poll_manual_download_queue(&mut self) {
        #[cfg(all(feature = "yt-dlp", feature = "backend-mpv"))]
        if !self.manual_downloads.blocked
            && !self.view.quitting
            && self.shutdown_persistence_succeeded.is_none()
            && !self.download_in_progress()
            && let Some((item, request)) = self.manual_downloads.deferred_cache.take()
        {
            if self.manual_downloads.active.is_some() {
                self.launch_manual_download(item, request.source_url, request.format);
            }
            return;
        }
        if self.manual_downloads.blocked
            || self.manual_downloads.active.is_some()
            || self.manual_downloads.choosing.is_some()
            || self.view.quitting
            || self.shutdown_persistence_succeeded.is_some()
        {
            return;
        }
        #[cfg(feature = "yt-dlp")]
        if self.download_in_progress()
            || self.pending_download_choice.is_some()
            || self.view.channel_download_popup.is_some()
        {
            return;
        }
        #[cfg(feature = "yandex-music")]
        if self.yandex_music_download_thread.is_some() {
            return;
        }
        let Some(entry) = self
            .manual_downloads
            .queue
            .entries
            .iter()
            .find(|entry| entry.state == DownloadQueueState::Queued)
            .cloned()
        else {
            return;
        };
        if entry.format.is_none() {
            self.manual_downloads.choosing = Some(entry.id);
            #[cfg(feature = "yt-dlp")]
            {
                self.choose_manual_download(self.manual_download_queue_item(&entry.source));
            }
            #[cfg(not(feature = "yt-dlp"))]
            self.finish_manual_download_choice(false);
            return;
        }
        if !self.begin_manual_download_attempt(entry.id) {
            return;
        }
        if entry.source.media_id.source == SourceKind::YandexMusic {
            #[cfg(feature = "yandex-music")]
            let result = self.start_queued_yandex_download(&entry.source);
            #[cfg(not(feature = "yandex-music"))]
            let result: Result<(), String> =
                Err("This build omits Yandex Music downloads".to_owned());
            if let Err(error) = result {
                self.finish_manual_download(Err(()));
                self.show_error_message("Queued download could not start", error);
            }
        } else {
            #[cfg(feature = "yt-dlp")]
            {
                let format = to_download_format(entry.format.expect("confirmed format"));
                self.launch_manual_download(
                    self.manual_download_queue_item(&entry.source),
                    entry.source.download_url,
                    format,
                );
            }
            #[cfg(not(feature = "yt-dlp"))]
            self.finish_manual_download(Err(()));
        }
    }

    /// Restores the current playback snapshot when it matches, retaining cache identity/artwork.
    #[cfg(feature = "yt-dlp")]
    fn manual_download_queue_item(&self, source: &DownloadSource) -> QueueItem {
        if let Some(item) = self
            .playback_queue
            .current()
            .filter(|item| item.media.id == source.media_id)
        {
            let mut item = item.clone();
            item.media.webpage_url = source.webpage_url.clone();
            item.media.title = source.title.clone();
            return item;
        }
        QueueItem {
            media: MediaItem {
                id: source.media_id.clone(),
                kind: source.kind,
                title: source.title.clone(),
                creator: source.creator.clone(),
                description: None,
                webpage_url: source.webpage_url.clone(),
                thumbnail_url: None,
                duration_seconds: source.duration_seconds,
                published_at: None,
                statistics: MediaStatistics::default(),
                license: MediaLicense::Unknown,
                chapters: Vec::new(),
                captions: Vec::new(),
            },
            playback_location: source.download_url.to_string(),
            start_at_seconds: None,
            added_at: unix_time(),
        }
    }

    /// Records the exact Archive derivative or YouTube stream choice before launching it.
    #[cfg(feature = "yt-dlp")]
    pub(super) fn capture_manual_download_choice(
        &mut self,
        item: &QueueItem,
        source: &url::Url,
        format: DownloadFormat,
    ) -> bool {
        let Some(id) = self.manual_downloads.choosing else {
            return true;
        };
        let mut queue = self.manual_downloads.queue.clone();
        let Some(entry) = queue
            .entries
            .iter_mut()
            .find(|entry| entry.id == id && entry.source.media_id == item.media.id)
        else {
            return false;
        };
        entry.source.download_url = source.clone();
        entry.format = Some(from_download_format(format));
        if !self.save_manual_download_queue(queue) {
            return false;
        }
        self.manual_downloads.choosing = None;
        if self.download_in_progress() {
            return false;
        }
        #[cfg(feature = "yandex-music")]
        if self.yandex_music_download_thread.is_some() {
            return false;
        }
        self.begin_manual_download_attempt(id)
    }

    /// Keeps podcast enclosures distinct from their browser pages during format selection.
    #[cfg(feature = "yt-dlp")]
    pub(super) fn captured_manual_download_url(&self, item: &QueueItem) -> url::Url {
        self.manual_downloads
            .choosing
            .and_then(|id| {
                self.manual_downloads
                    .queue
                    .entries
                    .iter()
                    .find(|entry| entry.id == id && entry.source.media_id == item.media.id)
            })
            .map_or_else(
                || item.media.webpage_url.clone(),
                |entry| entry.source.download_url.clone(),
            )
    }

    /// Advances the durable attempt before process creation, never after it.
    fn begin_manual_download_attempt(&mut self, id: u64) -> bool {
        let mut queue = self.manual_downloads.queue.clone();
        let Some(entry) = queue
            .entries
            .iter_mut()
            .find(|entry| entry.id == id && entry.state == DownloadQueueState::Queued)
        else {
            return false;
        };
        let Some(attempt) = entry.attempt.checked_add(1) else {
            return false;
        };
        entry.attempt = attempt;
        entry.state = DownloadQueueState::Running;
        if !self.save_manual_download_queue(queue) {
            return false;
        }
        self.manual_downloads.active = Some((id, attempt));
        true
    }

    /// Ends a chooser on error or explicit dismissal; other queued jobs remain intact.
    pub(super) fn finish_manual_download_choice(&mut self, cancelled: bool) {
        let Some(id) = self.manual_downloads.choosing.take() else {
            return;
        };
        self.change_queued_download_state(
            id,
            if cancelled {
                DownloadQueueState::Cancelled
            } else {
                DownloadQueueState::Failed
            },
        );
    }

    /// Completes the currently serialized transfer; asynchronous callers must check their owner first.
    pub(super) fn finish_manual_download(&mut self, result: Result<PathBuf, ()>) {
        if let Some(owner) = self.manual_downloads.active {
            self.finish_manual_download_for(owner, result);
        }
    }

    /// Ignores stale worker outcomes and stores only validated paths relative to the download directory.
    pub(super) fn finish_manual_download_for(
        &mut self,
        owner: (u64, u64),
        result: Result<PathBuf, ()>,
    ) {
        if self.manual_downloads.active != Some(owner) {
            return;
        }
        let path = result
            .ok()
            .and_then(|path| validated_relative_download(&self.config.downloads_dir(), &path));
        let mut queue = self.manual_downloads.queue.clone();
        let Some(entry) = queue.entries.iter_mut().find(|entry| {
            entry.id == owner.0
                && entry.attempt == owner.1
                && entry.state == DownloadQueueState::Running
        }) else {
            return;
        };
        entry.state = if path.is_some() {
            DownloadQueueState::Completed
        } else {
            DownloadQueueState::Failed
        };
        entry.completed_path = path;
        self.manual_downloads.active = None;
        self.manual_downloads.yandex_generation = None;
        self.save_manual_download_queue(queue);
        self.refresh_download_markers(true);
    }

    /// Marks explicit cancellation independently from shutdown interruption/recovery.
    #[cfg(any(feature = "yt-dlp", feature = "yandex-music"))]
    pub(super) fn cancel_manual_download_owner(&mut self) {
        #[cfg(all(feature = "yt-dlp", feature = "backend-mpv"))]
        {
            self.manual_downloads.deferred_cache = None;
        }
        if let Some((id, _)) = self.manual_downloads.active.take() {
            self.manual_downloads.yandex_generation = None;
            self.change_queued_download_state(id, DownloadQueueState::Cancelled);
        }
    }

    #[cfg(feature = "yandex-music")]
    pub(super) fn queued_yandex_download_started(&mut self, generation: u64) {
        self.manual_downloads.yandex_generation = Some(generation);
    }

    #[cfg(feature = "yandex-music")]
    pub(super) fn queued_yandex_download_is_current(&self, generation: u64) -> bool {
        self.manual_downloads.active.is_some()
            && self.manual_downloads.yandex_generation == Some(generation)
    }

    #[cfg(feature = "yandex-music")]
    pub(super) fn finish_queued_yandex_download(
        &mut self,
        generation: u64,
        result: Result<PathBuf, ()>,
    ) {
        if self.queued_yandex_download_is_current(generation) {
            self.finish_manual_download(result);
        }
    }

    fn change_queued_download_state(&mut self, id: u64, state: DownloadQueueState) {
        let mut queue = self.manual_downloads.queue.clone();
        if let Some(entry) = queue.entries.iter_mut().find(|entry| entry.id == id) {
            entry.state = state;
            entry.completed_path = None;
            self.save_manual_download_queue(queue);
        }
    }

    /// Retries a stopped job with its previously confirmed format; no implicit format substitution.
    pub(super) fn retry_queued_download(&mut self, id: u64) {
        if self.manual_downloads.blocked {
            return;
        }
        if self.manual_downloads.queue.entries.iter().any(|entry| {
            entry.id == id
                && matches!(
                    entry.state,
                    DownloadQueueState::Failed | DownloadQueueState::Cancelled
                )
        }) {
            self.change_queued_download_state(id, DownloadQueueState::Queued);
            self.poll_manual_download_queue();
        }
    }

    /// Cancels one selected pending/active job without discarding the rest of the batch.
    pub(super) fn cancel_queued_download(&mut self, id: u64) {
        if self.manual_downloads.blocked {
            return;
        }
        if self
            .manual_downloads
            .active
            .is_some_and(|owner| owner.0 == id)
        {
            #[cfg(feature = "yandex-music")]
            if self.cancel_yandex_music_download() {
                return;
            }
            #[cfg(feature = "yt-dlp")]
            self.cancel_active_download();
        } else if self.manual_downloads.choosing == Some(id) {
            #[cfg(feature = "yt-dlp")]
            self.dismiss_download_choice();
            #[cfg(not(feature = "yt-dlp"))]
            self.finish_manual_download_choice(true);
        } else if self
            .manual_downloads
            .queue
            .entries
            .iter()
            .any(|entry| entry.id == id && entry.state == DownloadQueueState::Queued)
        {
            self.change_queued_download_state(id, DownloadQueueState::Cancelled);
        }
    }

    /// Projects stable job IDs into the shared popup; selection survives status updates.
    pub(super) fn refresh_download_queue_popup(&mut self) {
        let Some(popup) = self.view.download_queue_popup.as_mut() else {
            return;
        };
        popup.entries = self
            .manual_downloads
            .queue
            .entries
            .iter()
            .map(|entry| DownloadQueueEntryView {
                id: entry.id,
                title: entry.source.title.clone(),
                state: match entry.state {
                    DownloadQueueState::Queued if entry.format.is_none() => {
                        "Waiting for format choice"
                    }
                    DownloadQueueState::Queued => "Queued",
                    DownloadQueueState::Running => "Downloading",
                    DownloadQueueState::Completed => "Downloaded",
                    DownloadQueueState::Failed => "Failed — retry available",
                    DownloadQueueState::Cancelled => "Cancelled — retry available",
                }
                .to_owned(),
            })
            .collect();
        popup.selected = popup.selected.min(popup.entries.len().saturating_sub(1));
    }

    pub(super) fn open_download_queue(&mut self) {
        self.view.download_queue_popup = Some(DownloadQueuePopupView {
            entries: Vec::new(),
            selected: 0,
        });
        self.refresh_download_queue_popup();
    }

    pub(super) fn select_download_queue_entry(&mut self, id: u64) {
        if let Some(popup) = self.view.download_queue_popup.as_mut()
            && let Some(index) = popup.entries.iter().position(|entry| entry.id == id)
        {
            popup.selected = index;
        }
    }

    /// Keeps a queued cache save pending without starting a duplicate network transfer.
    #[cfg(all(feature = "yt-dlp", feature = "backend-mpv"))]
    pub(super) fn defer_manual_download_cache(
        &mut self,
        item: &QueueItem,
        request: &DownloadRequest,
    ) {
        if self.manual_downloads.active.is_some() {
            self.manual_downloads.deferred_cache = Some((item.clone(), request.clone()));
            self.view.download = Some(DownloadView {
                title: item.media.title.clone(),
                active: true,
                ..DownloadView::default()
            });
        }
    }

    /// Rechecks local files at most once per five seconds, not on every repaint.
    pub(super) fn refresh_download_markers(&mut self, force: bool) {
        if force
            || self
                .manual_downloads
                .marker_check
                .is_none_or(|time| time.elapsed() >= Duration::from_secs(5))
        {
            self.manual_downloads.downloaded = self
                .manual_downloads
                .queue
                .entries
                .iter()
                .filter(|entry| entry.state == DownloadQueueState::Completed)
                .filter_map(|entry| {
                    let relative = entry.completed_path.as_ref()?;
                    validated_relative_download(
                        &self.config.downloads_dir(),
                        &self.config.downloads_dir().join(relative),
                    )
                    .map(|_| entry.source.media_id.clone())
                })
                .collect();
            self.manual_downloads.marker_check = Some(Instant::now());
        }
        let marked: HashSet<_> = self
            .manual_downloads
            .marks
            .iter()
            .map(|source| &source.media_id)
            .collect();
        for row in self
            .view
            .rows
            .iter_mut()
            .chain(self.view.subscriptions.items.iter_mut())
        {
            row.download_marked = row.media_id.as_ref().is_some_and(|id| marked.contains(id));
            row.downloaded = row
                .media_id
                .as_ref()
                .is_some_and(|id| self.manual_downloads.downloaded.contains(id));
        }
    }

    /// Preserves interrupted intent after workers have stopped, and reports failed durability barriers.
    pub(super) fn shutdown_manual_downloads(&mut self) -> bool {
        let mut queue = self
            .manual_downloads
            .pending_write
            .clone()
            .unwrap_or_else(|| self.manual_downloads.queue.clone());
        let recovered = queue.recover_running();
        self.manual_downloads.active = None;
        self.manual_downloads.yandex_generation = None;
        if recovered || self.manual_downloads.pending_write.is_some() {
            self.save_manual_download_queue(queue)
        } else {
            !self.manual_downloads.blocked
        }
    }
}

/// Creates a pending intent; native Yandex downloads always preserve their original encoding.
fn new_entry(id: u64, source: DownloadSource) -> DownloadQueueEntry {
    let format = (source.media_id.source == SourceKind::YandexMusic)
        .then_some(QueuedDownloadFormat::YandexOriginal);
    DownloadQueueEntry {
        id,
        source,
        format,
        state: DownloadQueueState::Queued,
        attempt: 0,
        completed_path: None,
    }
}

/// Rejects symlink escapes, missing/empty outputs, and paths outside the user's downloads directory.
fn validated_relative_download(directory: &Path, path: &Path) -> Option<PathBuf> {
    let root = crate::fs_path::canonicalize(directory).ok()?;
    let resolved = crate::fs_path::canonicalize(path).ok()?;
    let relative = resolved.strip_prefix(&root).ok()?.to_owned();
    let metadata = std::fs::metadata(resolved).ok()?;
    (metadata.is_file() && metadata.len() > 0 && !relative.as_os_str().is_empty())
        .then_some(relative)
}

#[cfg(feature = "yt-dlp")]
fn from_download_format(format: DownloadFormat) -> QueuedDownloadFormat {
    match format {
        DownloadFormat::ExactFile => QueuedDownloadFormat::ExactFile,
        DownloadFormat::BestVideo => QueuedDownloadFormat::BestVideo,
        DownloadFormat::AudioOnlyWithoutReencoding => {
            QueuedDownloadFormat::AudioOnlyWithoutReencoding
        }
        DownloadFormat::OpusWithoutTranscoding => QueuedDownloadFormat::OpusWithoutTranscoding,
        DownloadFormat::OriginalBestAudio => QueuedDownloadFormat::OriginalBestAudio,
        DownloadFormat::TranscodeToOpus => QueuedDownloadFormat::TranscodeToOpus,
    }
}

#[cfg(feature = "yt-dlp")]
fn to_download_format(format: QueuedDownloadFormat) -> DownloadFormat {
    match format {
        QueuedDownloadFormat::ExactFile => DownloadFormat::ExactFile,
        QueuedDownloadFormat::BestVideo => DownloadFormat::BestVideo,
        QueuedDownloadFormat::AudioOnlyWithoutReencoding => {
            DownloadFormat::AudioOnlyWithoutReencoding
        }
        QueuedDownloadFormat::OpusWithoutTranscoding => DownloadFormat::OpusWithoutTranscoding,
        QueuedDownloadFormat::OriginalBestAudio | QueuedDownloadFormat::YandexOriginal => {
            DownloadFormat::OriginalBestAudio
        }
        QueuedDownloadFormat::TranscodeToOpus => DownloadFormat::TranscodeToOpus,
    }
}
