//! Exact original-file cache ownership, separate from mpv's remuxed packet cache.

use super::*;
use crate::app::cached_download::{
    CachePreparationGate, CachedDownloadArtifact, CachedDownloadJob, CachedDownloadPublished,
    CachedThumbnail, PREPARING_CACHE, prepare_cached_thumbnail,
};
use crate::archive_playback_cache::{ArchivePlaybackCache, CompletedOriginal};
use crate::original_cache_download::{PreparedOriginalDownload, prepare_cached_original};
use std::sync::atomic::Ordering;

impl AppController {
    /// Replaces only the transient player input, never Queue or History identifiers.
    pub(super) fn cache_original_playback_input(
        &mut self,
        item: &QueueItem,
        input: &mut PlaybackInput,
    ) {
        let source = url::Url::parse(&item.playback_location)
            .ok()
            .filter(|source| {
                item.media.id.source == SourceKind::ArchiveOrg
                    && matches!(
                        item.media.kind,
                        MediaKind::Audio | MediaKind::PodcastEpisode
                    )
                    && input.location == item.playback_location
                    && input.http_headers.is_empty()
                    && crate::domain::is_canonical_archive_org_audio_url(source)
            });
        let Some(source) = source else {
            self.archive_playback_cache = None;
            return;
        };
        if self
            .archive_playback_cache
            .as_ref()
            .is_none_or(|cache| cache.source_url() != &source)
        {
            self.archive_playback_cache = None;
            #[cfg(feature = "cache")]
            let ram_backed = input.cache_identity.as_deref().and_then(|identity| {
                let ram = self.player.as_ref()?.ram_cache_handle()?;
                let route = ram.register_trusted(identity, source.as_str(), &input.http_headers)?;
                ArchivePlaybackCache::start_with_ram_cache(source.clone(), route).ok()
            });
            #[cfg(not(feature = "cache"))]
            let ram_backed = None;
            self.archive_playback_cache =
                ram_backed.or_else(|| ArchivePlaybackCache::start(source).ok());
        }
        if let Some(cache) = &self.archive_playback_cache {
            input.location = cache.playback_url().to_owned();
            input.bypass_ytdl = true;
            // The backend tracks RAM failures without bypassing this original
            // proxy. Otherwise a later short read can look like ordinary EOF
            // and silently finish the track instead of retrying its source.
            input.cache_fallback_url = None;
            #[cfg(feature = "cache")]
            if cache.has_ram_upstream() {
                input.cache_fallback_url = Some(cache.source_url().to_string());
            }
            if input.cache_fallback_url.is_none() {
                input.cache_identity = None;
            }
        }
    }

    /// Reuses only an exact complete original; incomplete/other variants use normal downloading.
    pub(super) fn try_cached_original_download(
        &mut self,
        item: &QueueItem,
        request: &DownloadRequest,
    ) -> bool {
        if !self.original_download_request_matches(item, request) {
            return false;
        }
        let Some(complete) = self
            .archive_playback_cache
            .as_ref()
            .filter(|cache| cache.source_url() == &request.source_url)
            .and_then(ArchivePlaybackCache::completed)
        else {
            return false;
        };
        let job = match OriginalDownloadJob::start(
            complete,
            &self.config,
            request,
            item.media.thumbnail_url.as_ref(),
        ) {
            Ok(Some(job)) => job,
            Ok(None) => return false,
            Err(()) => {
                self.view.status_line =
                    "Previous cache save is still stopping; try again shortly".to_owned();
                self.defer_manual_download_cache(item, request);
                return true;
            }
        };
        self.begin_cached_download(item, request, Box::new(job));
        true
    }

    /// Requires the exact playing file and an unchanged original-file download intent.
    fn original_download_request_matches(
        &self,
        item: &QueueItem,
        request: &DownloadRequest,
    ) -> bool {
        item.media.id.source == SourceKind::ArchiveOrg
            && matches!(
                item.media.kind,
                MediaKind::Audio | MediaKind::PodcastEpisode
            )
            && self.current_media.as_ref() == Some(&item.media.id)
            && self.playback_phase == PlaybackPhase::Playing
            && !self.view.playback.idle
            && !self.view.playback.live
            && request.scope == DownloadScope::SingleItem
            && request.format == DownloadFormat::ExactFile
            && request.source_url.as_str() == item.playback_location
            && crate::domain::is_canonical_archive_org_audio_url(&request.source_url)
            && self.playback_queue.current().is_some_and(|current| {
                current.media.id == item.media.id
                    && current.playback_location == item.playback_location
            })
    }
}

/// A foreground download slot with bounded off-thread disk copying and cancellation.
struct OriginalDownloadJob {
    receiver: Receiver<Result<Box<dyn CachedDownloadArtifact>, ()>>,
    cancellation: Arc<AtomicBool>,
    notifier: WorkerNotifier,
}

impl OriginalDownloadJob {
    fn start(
        complete: CompletedOriginal,
        config: &Config,
        request: &DownloadRequest,
        thumbnail: Option<&url::Url>,
    ) -> Result<Option<Self>, ()> {
        if !complete.is_current() || complete.source_url() != &request.source_url {
            return Ok(None);
        }
        let config = config.clone();
        let destination = request.destination.clone();
        let thumbnail = thumbnail.cloned();
        Self::start_worker(&PREPARING_CACHE, move |worker_cancel| {
            if !complete.is_current() {
                return Err(());
            }
            let prepared = prepare_cached_original(
                complete.path(),
                complete.len(),
                complete.source_url(),
                &destination,
                &worker_cancel,
            )
            .map_err(|_| ())?;
            let thumbnail_requested =
                config.subscriptions.download_thumbnails && thumbnail.is_some();
            let thumbnail = thumbnail_requested
                .then(|| prepare_cached_thumbnail(&config, thumbnail.as_ref(), &destination))
                .flatten();
            if worker_cancel.load(Ordering::Acquire) || !complete.is_current() {
                return Err(());
            }
            Ok(Box::new(OriginalDownloadArtifact {
                prepared,
                complete,
                cancellation: worker_cancel,
                thumbnail,
                thumbnail_requested,
            }) as Box<dyn CachedDownloadArtifact>)
        })
    }

    /// Starts the copy worker; an occupied gate is distinct from a cache miss.
    fn start_worker(
        gate: &CachePreparationGate,
        work: impl FnOnce(Arc<AtomicBool>) -> Result<Box<dyn CachedDownloadArtifact>, ()>
        + Send
        + 'static,
    ) -> Result<Option<Self>, ()> {
        let permit = gate.try_acquire().ok_or(())?;
        let (sender, receiver) = bounded(1);
        let notifier = WorkerNotifier::default();
        let sender = ResponseSender::new(sender, notifier.clone());
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancellation);
        let worker = thread::Builder::new()
            .name("youta-original-download".to_owned())
            .spawn(move || {
                // Cancellation can defer the next owner until this slot is released.
                let responses = sender;
                let _permit = permit;
                let result = work(worker_cancel);
                let _ = responses.send(result);
            });
        if worker.is_err() {
            return Ok(None);
        }
        Ok(Some(Self {
            receiver,
            cancellation,
            notifier,
        }))
    }
}

impl CachedDownloadJob for OriginalDownloadJob {
    fn set_worker_waker(&mut self, waker: Option<std::task::Waker>) {
        self.notifier.set_waker(waker);
    }

    fn poll(&mut self) -> Option<Result<Box<dyn CachedDownloadArtifact>, ()>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(crossbeam_channel::TryRecvError::Empty) => None,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err(())),
        }
    }
}

impl Drop for OriginalDownloadJob {
    fn drop(&mut self) {
        // Keep the relay until retirement; the controller detaches dead frontends.
        self.cancellation.store(true, Ordering::Release);
    }
}

/// Holds the exact cache owner until the controller authorizes final publication.
struct OriginalDownloadArtifact {
    prepared: PreparedOriginalDownload,
    complete: CompletedOriginal,
    cancellation: Arc<AtomicBool>,
    thumbnail: Option<CachedThumbnail>,
    thumbnail_requested: bool,
}

impl CachedDownloadArtifact for OriginalDownloadArtifact {
    fn publish(
        self: Box<Self>,
        destination: &Path,
        title: &str,
        id: &str,
    ) -> Result<CachedDownloadPublished, String> {
        if self.cancellation.load(Ordering::Acquire) || !self.complete.is_current() {
            return Err("original playback cache changed before publication".to_owned());
        }
        let path = self.prepared.publish(destination, title, id)?;
        let thumbnail_saved = self
            .thumbnail
            .is_some_and(|thumbnail| thumbnail.publish(&path));
        Ok(CachedDownloadPublished {
            path,
            thumbnail_missing: self.thumbnail_requested && !thumbnail_saved,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordedWake(Sender<()>);

    impl std::task::Wake for RecordedWake {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    /// The callback channel is unbounded so notification itself cannot block a worker.
    fn recorded_wake() -> (std::task::Waker, Receiver<()>) {
        let (sender, receiver) = unbounded();
        (
            std::task::Waker::from(Arc::new(RecordedWake(sender))),
            receiver,
        )
    }

    /// Successful preparation can be tested without publishing or touching any file.
    struct UnpublishedArtifact;

    impl CachedDownloadArtifact for UnpublishedArtifact {
        fn publish(
            self: Box<Self>,
            _: &Path,
            _: &str,
            _: &str,
        ) -> Result<CachedDownloadPublished, String> {
            panic!("notification tests never publish prepared artifacts");
        }
    }

    #[test]
    fn original_download_wake_replays_work_completed_before_registration() {
        let gate = CachePreparationGate::default();
        let mut job = OriginalDownloadJob::start_worker(&gate, |_| Err(()))
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while job.receiver.is_empty() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        let (waker, wakes) = recorded_wake();
        job.set_worker_waker(Some(waker));
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(job.poll(), Some(Err(()))));
    }

    #[test]
    fn original_download_wake_announces_success_failure_and_unwind() {
        for outcome in 0..3 {
            let gate = CachePreparationGate::default();
            let (release, held) = bounded::<()>(1);
            let mut job = OriginalDownloadJob::start_worker(&gate, move |_| {
                held.recv_timeout(Duration::from_secs(5)).unwrap();
                match outcome {
                    0 => Ok(Box::new(UnpublishedArtifact)),
                    1 => Err(()),
                    // Exercise unwinding and disconnect notification, not the global
                    // panic hook: Windows CI backtrace symbolization can exceed the
                    // bounded wake deadline before any destructor gets to run.
                    _ => std::panic::resume_unwind(Box::new(
                        "simulated original download worker failure",
                    )),
                }
            })
            .unwrap()
            .unwrap();
            let (waker, wakes) = recorded_wake();
            job.set_worker_waker(Some(waker));
            wakes.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(job.poll().is_none());
            release.send(()).unwrap();
            wakes.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(job.poll(), Some(result) if result.is_ok() == (outcome == 0)));
        }
    }

    #[test]
    fn original_download_wake_retires_cancelled_worker_after_releasing_its_gate() {
        let gate = CachePreparationGate::default();
        let (release, held) = bounded::<()>(1);
        let mut job = OriginalDownloadJob::start_worker(&gate, move |_| {
            let _ = held.recv_timeout(Duration::from_secs(5));
            Err(())
        })
        .unwrap()
        .unwrap();
        let (waker, wakes) = recorded_wake();
        job.set_worker_waker(Some(waker));
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        let cancelled = Arc::clone(&job.cancellation);
        drop(job);
        assert!(cancelled.load(Ordering::Acquire));
        assert!(
            gate.try_acquire().is_none(),
            "the cancelled worker still owns its slot"
        );
        release.send(()).unwrap();
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            gate.try_acquire().is_some(),
            "retirement wake follows slot release so a deferred download can start"
        );
        assert!(matches!(
            wakes.recv_timeout(Duration::from_secs(5)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ));
    }

    /// A packet-cache worker and an original-file worker share one process resource slot.
    #[test]
    fn occupied_preparation_gate_refuses_original_copy_without_starting_work() {
        let gate = CachePreparationGate::default();
        let _existing = gate.try_acquire().unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = Arc::clone(&started);
        let result = OriginalDownloadJob::start_worker(&gate, move |_| {
            worker_started.store(true, Ordering::Release);
            Err(())
        });
        assert!(result.is_err(), "occupied preparation is not a cache miss");
        assert!(!started.load(Ordering::Acquire));
    }

    /// Cancelling a foreground owner must not release a worker still finishing disk I/O.
    #[test]
    fn cancelled_original_copy_holds_preparation_gate_until_worker_retires() {
        let gate = CachePreparationGate::default();
        let (started, worker_started) = bounded(1);
        let (finish, worker_finish) = bounded(1);
        let job = OriginalDownloadJob::start_worker(&gate, move |cancelled| {
            started.send(cancelled).unwrap();
            let _ = worker_finish.recv_timeout(Duration::from_secs(5));
            Err(())
        })
        .unwrap()
        .unwrap();
        let cancelled = worker_started.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(job);
        assert!(cancelled.load(Ordering::Acquire));
        assert!(
            gate.try_acquire().is_none(),
            "retiring copy still owns its slot"
        );
        finish.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(permit) = gate.try_acquire() {
                drop(permit);
                break;
            }
            assert!(Instant::now() < deadline, "retired worker leaked its slot");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// No real player, provider, or upstream is needed to reproduce the rejected MP3 intent.
    #[test]
    fn original_archive_mp3_is_eligible_but_other_variants_and_formats_are_not() {
        let config = Config::for_dir("/tmp/youta-original-intent-fixture");
        let mut controller =
            AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        let source = url::Url::parse("https://archive.org/download/fixture/original.mp3").unwrap();
        let media = MediaItem {
            id: MediaId::new(SourceKind::ArchiveOrg, source.as_str()),
            title: "Original tagged MP3".to_owned(),
            kind: MediaKind::Audio,
            webpage_url: source.clone(),
            creator: None,
            description: None,
            thumbnail_url: None,
            duration_seconds: Some(319),
            published_at: None,
            statistics: Default::default(),
            license: Default::default(),
            chapters: Vec::new(),
            captions: Vec::new(),
        };
        let item = QueueItem {
            media,
            playback_location: source.to_string(),
            start_at_seconds: None,
            added_at: 0,
        };
        controller.playback_queue.begin_now(item.clone(), false);
        controller.current_media = Some(item.media.id.clone());
        controller.playback_phase = PlaybackPhase::Playing;
        controller.view.playback.idle = false;
        let mut request = DownloadRequest {
            source_url: source,
            destination: PathBuf::from("/tmp/unused-original-destination"),
            format: DownloadFormat::ExactFile,
            scope: DownloadScope::SingleItem,
            playlist_start: None,
            skip_shorts: false,
            write_thumbnail: false,
            archive_path: None,
        };
        assert!(controller.original_download_request_matches(&item, &request));
        request.source_url =
            url::Url::parse("https://archive.org/download/fixture/original.flac").unwrap();
        assert!(!controller.original_download_request_matches(&item, &request));
        request.source_url = item.media.webpage_url.clone();
        request.format = DownloadFormat::AudioOnlyWithoutReencoding;
        assert!(!controller.original_download_request_matches(&item, &request));
        request.format = DownloadFormat::ExactFile;
        controller.current_media = None;
        assert!(!controller.original_download_request_matches(&item, &request));
    }
}
