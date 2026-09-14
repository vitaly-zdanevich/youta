//! Best-effort playback-cache downloads with one cancellable, nonblocking owner.

use super::*;
use crate::playback::cache_export::{CacheExportedFile, PlaybackCacheHandle};
use crate::playback_cache_download::{PreparedCacheDownload, prepare_cached_opus};
use std::sync::atomic::Ordering;

/// One preparation across cancellation/restart bursts, including validator cleanup.
pub(super) static PREPARING_CACHE: std::sync::LazyLock<CachePreparationGate> =
    std::sync::LazyLock::new(CachePreparationGate::default);

/// Shared by packet-cache and original-byte workers, including cancelled retirement.
#[derive(Default)]
pub(super) struct CachePreparationGate(Arc<AtomicBool>);

impl CachePreparationGate {
    /// Reserves one worker without waiting for disk I/O or a preceding cancellation.
    pub(super) fn try_acquire(&self) -> Option<PreparationPermit> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(PreparationPermit(Arc::clone(&self.0)))
    }
}

/// Retains exclusion until the worker exits, not merely until its UI owner drops.
pub(super) struct PreparationPermit(Arc<AtomicBool>);

impl Drop for PreparationPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A private, verified result whose publication is controlled by the UI reducer.
pub(super) trait CachedDownloadArtifact: Send {
    /// Publishes without replacing a file, after validating the playback epoch.
    fn publish(
        self: Box<Self>,
        destination: &Path,
        title: &str,
        id: &str,
    ) -> Result<CachedDownloadPublished, String>;
}

/// A committed audio path plus an optional, nonfatal thumbnail-cache miss.
pub(super) struct CachedDownloadPublished {
    pub path: PathBuf,
    pub thumbnail_missing: bool,
}

/// A bounded background cache attempt; dropping it cancels without joining the UI.
pub(super) trait CachedDownloadJob: Send {
    /// Returns once; failure means normal downloading may be attempted instead.
    fn poll(&mut self) -> Option<Result<Box<dyn CachedDownloadArtifact>, ()>>;
}

/// Injectable boundary for native cache export and offline media verification.
pub(super) trait CachedDownloadService: Send {
    /// Returns no job when this backend has no safely reusable cache ticket.
    fn start(
        &mut self,
        player: &dyn PlaybackBackend,
        config: &Config,
        destination: &Path,
        thumbnail: Option<&url::Url>,
    ) -> Option<Box<dyn CachedDownloadJob>>;
}

/// Production mpv cache preparation, independent of the ordinary download child.
pub(super) struct SystemCachedDownloadService;

impl CachedDownloadService for SystemCachedDownloadService {
    fn start(
        &mut self,
        player: &dyn PlaybackBackend,
        config: &Config,
        destination: &Path,
        thumbnail: Option<&url::Url>,
    ) -> Option<Box<dyn CachedDownloadJob>> {
        let handle = player.cache_export_handle()?;
        let permit = PREPARING_CACHE.try_acquire()?;
        let cancellation = Arc::new(AtomicBool::new(false));
        let export = handle.start(Arc::clone(&cancellation))?;
        let (sender, receiver) = bounded(1);
        let config = config.clone();
        let destination = destination.to_owned();
        let thumbnail = thumbnail.cloned();
        let worker_cancel = Arc::clone(&cancellation);
        thread::Builder::new()
            .name("youta-cache-download".to_owned())
            .spawn(move || {
                let _permit = permit;
                let result = (|| {
                    let exported = export.wait(&worker_cancel).map_err(|_| ())?;
                    if !exported.is_current() || worker_cancel.load(Ordering::Acquire) {
                        return Err(());
                    }
                    let prepared = prepare_cached_opus(
                        &config,
                        exported.path(),
                        exported.duration(),
                        &destination,
                        &worker_cancel,
                    )
                    .map_err(|_| ())?;
                    let thumbnail_requested =
                        config.subscriptions.download_thumbnails && thumbnail.is_some();
                    let cached_thumbnail = thumbnail_requested
                        .then(|| {
                            prepare_cached_thumbnail(&config, thumbnail.as_ref(), &destination)
                        })
                        .flatten();
                    if !exported.is_current() || worker_cancel.load(Ordering::Acquire) {
                        return Err(());
                    }
                    Ok(Box::new(SystemCachedArtifact {
                        prepared,
                        exported,
                        cancellation: worker_cancel,
                        thumbnail: cached_thumbnail,
                        thumbnail_requested,
                    }) as Box<dyn CachedDownloadArtifact>)
                })();
                let _ = sender.send(result);
            })
            .ok()?;
        Some(Box::new(SystemCachedJob {
            receiver,
            cancellation,
            _handle: handle,
        }))
    }
}

struct SystemCachedJob {
    receiver: Receiver<Result<Box<dyn CachedDownloadArtifact>, ()>>,
    cancellation: Arc<AtomicBool>,
    _handle: PlaybackCacheHandle,
}

impl CachedDownloadJob for SystemCachedJob {
    fn poll(&mut self) -> Option<Result<Box<dyn CachedDownloadArtifact>, ()>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(crossbeam_channel::TryRecvError::Empty) => None,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err(())),
        }
    }
}

impl Drop for SystemCachedJob {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
    }
}

struct SystemCachedArtifact {
    prepared: PreparedCacheDownload,
    exported: CacheExportedFile,
    cancellation: Arc<AtomicBool>,
    thumbnail: Option<CachedThumbnail>,
    thumbnail_requested: bool,
}

impl CachedDownloadArtifact for SystemCachedArtifact {
    fn publish(
        self: Box<Self>,
        destination: &Path,
        title: &str,
        id: &str,
    ) -> Result<CachedDownloadPublished, String> {
        if self.cancellation.load(Ordering::Acquire) || !self.exported.is_current() {
            return Err("Playback cache changed before publication".to_owned());
        }
        let path = self.prepared.publish(destination, title, id)?;
        // An optional image never causes the now-committed audio to be downloaded again.
        let thumbnail_saved = self
            .thumbnail
            .is_some_and(|thumbnail| thumbnail.publish(&path));
        Ok(CachedDownloadPublished {
            path,
            thumbnail_missing: self.thumbnail_requested && !thumbnail_saved,
        })
    }
}

/// Already downloaded artwork only: an offline audio save must remain offline.
pub(super) struct CachedThumbnail {
    file: tempfile::NamedTempFile,
    extension: &'static str,
}

impl CachedThumbnail {
    /// Adds an already cached sidecar without replacing an existing file.
    pub(super) fn publish(self, audio_path: &Path) -> bool {
        std::fs::hard_link(self.file.path(), audio_path.with_extension(self.extension)).is_ok()
    }
}

#[cfg(feature = "remote-artwork")]
pub(super) fn prepare_cached_thumbnail(
    config: &Config,
    source: Option<&url::Url>,
    destination: &Path,
) -> Option<CachedThumbnail> {
    use std::io::Write;
    let bytes = crate::artwork::ThumbnailCache::new(config.thumbnail_cache_dir())
        .read(source?)
        .ok()??;
    let extension = match crate::artwork::ArtworkFormat::sniff(&bytes)? {
        crate::artwork::ArtworkFormat::Jpeg => "jpg",
        crate::artwork::ArtworkFormat::Png => "png",
        crate::artwork::ArtworkFormat::WebP => "webp",
    };
    let mut file = tempfile::Builder::new()
        .prefix(".youta-cached-thumbnail-")
        .tempfile_in(destination)
        .ok()?;
    file.write_all(&bytes).ok()?;
    file.as_file().sync_all().ok()?;
    Some(CachedThumbnail { file, extension })
}

#[cfg(not(feature = "remote-artwork"))]
pub(super) fn prepare_cached_thumbnail(
    _config: &Config,
    _source: Option<&url::Url>,
    _destination: &Path,
) -> Option<CachedThumbnail> {
    None
}

/// Frozen download intent retained across asynchronous export and validation.
pub(super) struct PendingCachedDownload {
    item: QueueItem,
    request: DownloadRequest,
    job: Box<dyn CachedDownloadJob>,
    /// Stable manual attempt, retained across a cache hit or network fallback.
    owner: Option<(u64, u64)>,
}

impl AppController {
    /// Attempts a complete cache for the exact playing Archive original or YouTube audio.
    pub(super) fn try_cached_manual_download(
        &mut self,
        item: &QueueItem,
        request: &DownloadRequest,
    ) -> bool {
        #[cfg(feature = "archive-org")]
        if self.try_cached_original_download(item, request) {
            return true;
        }
        if item.media.id.source != SourceKind::YouTube
            || !matches!(
                item.media.kind,
                MediaKind::Audio | MediaKind::Video | MediaKind::PodcastEpisode
            )
            || self.current_media.as_ref() != Some(&item.media.id)
            || self.playback_phase != PlaybackPhase::Playing
            || self.view.playback.idle
            || self.view.playback.live
            || request.scope != DownloadScope::SingleItem
            || !matches!(
                request.format,
                DownloadFormat::OpusWithoutTranscoding | DownloadFormat::AudioOnlyWithoutReencoding
            )
            || request.source_url != item.media.webpage_url
            || self.playback_queue.current().is_none_or(|current| {
                current.media.id != item.media.id
                    || current.playback_location != item.playback_location
            })
        {
            return false;
        }
        let Some(player) = self.player.as_deref() else {
            return false;
        };
        let Some(job) = self.cached_download_service.start(
            player,
            &self.config,
            &request.destination,
            item.media.thumbnail_url.as_ref(),
        ) else {
            return false;
        };
        self.begin_cached_download(item, request, job);
        true
    }

    /// Gives original-file and packet-cache jobs the same foreground/cancellation owner.
    pub(super) fn begin_cached_download(
        &mut self,
        item: &QueueItem,
        request: &DownloadRequest,
        job: Box<dyn CachedDownloadJob>,
    ) {
        self.pending_cached_download = Some(PendingCachedDownload {
            item: item.clone(),
            request: request.clone(),
            job,
            owner: self.manual_downloads.active,
        });
        self.download_cancellation_notice_deadline = None;
        self.view.download = Some(DownloadView {
            title: item.media.title.clone(),
            active: true,
            ..DownloadView::default()
        });
        self.view.status_line = format!("Checking the playback cache for {}…", item.media.title);
    }

    /// Consumes one cache outcome; a miss starts the captured normal download once.
    pub(super) fn poll_cached_download(&mut self, now: Instant) {
        let Some(pending) = self.pending_cached_download.as_mut() else {
            return;
        };
        let Some(result) = pending.job.poll() else {
            return;
        };
        let pending = self
            .pending_cached_download
            .take()
            .expect("polled cache job");
        if pending.owner.is_some() && pending.owner != self.manual_downloads.active {
            return;
        }
        // Keep the job alive until publication: dropping it requests cancellation.
        let published = result
            .ok()
            .filter(|_| self.current_media.as_ref() == Some(&pending.item.media.id))
            .and_then(|artifact| {
                artifact
                    .publish(
                        &pending.request.destination,
                        &pending.item.media.title,
                        &pending.item.media.id.external_id,
                    )
                    .ok()
            });
        if let Some(published) = published {
            let bytes = std::fs::metadata(&published.path).map_or(0, |metadata| metadata.len());
            self.view.download = Some(DownloadView {
                title: pending.item.media.title,
                downloaded_bytes: bytes,
                total_bytes: Some(bytes),
                completed_files: 1,
                active: false,
                completed_path: Some(published.path.display().to_string()),
                eta_seconds: Some(0),
                ..DownloadView::default()
            });
            self.download_completion_notice_deadline =
                Some(now + DOWNLOAD_COMPLETION_NOTICE_DURATION);
            if let Some(owner) = pending.owner {
                self.finish_manual_download_for(owner, Ok(published.path.clone()));
            }
            if self.view.screen == Screen::Downloaded {
                self.populate_downloads();
                self.refresh_selected_playlist_state();
            }
            self.view.status_line = format!(
                "Saved from playback cache: {}{}",
                published.path.display(),
                if published.thumbnail_missing {
                    " (thumbnail was not cached)"
                } else {
                    ""
                }
            );
        } else {
            self.start_prepared_manual_download(pending.item, pending.request);
        }
    }

    /// Drops/cancels the sole cache worker; no network fallback follows cancellation.
    pub(super) fn cancel_cached_download_at(&mut self, now: Instant) -> bool {
        let Some(pending) = self.pending_cached_download.take() else {
            return false;
        };
        mark_download_inactive(&mut self.view);
        self.download_completion_notice_deadline = None;
        self.download_cancellation_notice_deadline =
            Some(now + DOWNLOAD_CANCELLATION_NOTICE_DURATION);
        self.view.status_line = format!("Cancelled download: {}", pending.item.media.title);
        if pending.owner.is_some() && pending.owner == self.manual_downloads.active {
            self.cancel_manual_download_owner();
        }
        true
    }
}

#[cfg(all(test, feature = "remote-artwork"))]
mod thumbnail_tests {
    use super::*;

    #[test]
    fn thumbnail_reuse_reads_only_existing_bytes_and_preserves_the_cache() {
        let directory = crate::test_support::canonical_tempdir("cached download thumbnail");
        let config = Config::for_dir(directory.path().join("config"));
        let destination = directory.path().join("downloads");
        std::fs::create_dir(&destination).unwrap();
        let source = url::Url::parse("https://example.invalid/thumbnail").unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        let cache = crate::artwork::ThumbnailCache::new(config.thumbnail_cache_dir());
        cache.prepare().unwrap();
        cache.store(&source, bytes).unwrap();
        let thumbnail = prepare_cached_thumbnail(&config, Some(&source), &destination).unwrap();
        let temporary = thumbnail.file.path().to_owned();
        assert_eq!(thumbnail.extension, "png");
        assert_eq!(std::fs::read(&temporary).unwrap(), bytes);
        assert_eq!(cache.read(&source).unwrap().unwrap(), bytes);
        drop(thumbnail);
        assert!(!temporary.exists());
        assert_eq!(cache.read(&source).unwrap().unwrap(), bytes);
    }

    #[test]
    fn missing_thumbnail_does_not_fetch_or_create_a_sidecar() {
        let directory = crate::test_support::canonical_tempdir("missing cached thumbnail");
        let config = Config::for_dir(directory.path().join("config"));
        let destination = directory.path().join("downloads");
        std::fs::create_dir(&destination).unwrap();
        let source = url::Url::parse("https://example.invalid/thumbnail").unwrap();
        assert!(prepare_cached_thumbnail(&config, Some(&source), &destination).is_none());
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    }
}
