use super::*;
use crate::app::cached_download::*;

struct DeferredCacheService;
struct DeferredCacheJob;

impl CachedDownloadJob for DeferredCacheJob {
    fn poll(&mut self) -> Option<Result<Box<dyn CachedDownloadArtifact>, ()>> {
        None
    }
}

impl CachedDownloadService for DeferredCacheService {
    fn start(
        &mut self,
        _player: &dyn PlaybackBackend,
        _config: &Config,
        _destination: &Path,
        _thumbnail: Option<&url::Url>,
    ) -> Option<Box<dyn CachedDownloadJob>> {
        Some(Box::new(DeferredCacheJob))
    }
}

#[test]
fn cached_download_attempt_precedes_a_second_network_download() {
    let directory = crate::test_support::canonical_tempdir("cached download controller");
    let config = Config::for_dir(directory.path().join("config"));
    let process = MockRunningDownload {
        progress: Some(Cursor::new(Vec::new())),
        errors: Some(Cursor::new(Vec::new())),
        exits: VecDeque::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (mut controller, requests, _) = controller_with_mock_download(config, process);
    let (mut playback_controller, _) = controller_with_mock_statuses([]);
    controller.playback_factory = playback_controller.playback_factory.take();
    let item = controller.selected_queue_item().unwrap();
    controller.play_queue_item(item.clone(), false);
    controller.playback_phase = PlaybackPhase::Playing;
    controller.view.playback.idle = false;
    controller.cached_download_service = Box::new(DeferredCacheService);
    let source = item.media.webpage_url.clone();
    controller.launch_manual_download(item, source, DownloadFormat::AudioOnlyWithoutReencoding);
    assert!(
        requests.lock().unwrap().is_empty(),
        "try the completed playback cache before fetching the audio again"
    );
    assert!(controller.pending_cached_download.is_some());
    assert!(controller.view.download.as_ref().unwrap().active);
    controller.shutdown();
}

type CacheResult = Arc<Mutex<Option<Result<Box<dyn CachedDownloadArtifact>, ()>>>>;

struct ControlledCacheService {
    result: CacheResult,
    dropped: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
}

struct ControlledCacheJob {
    result: CacheResult,
    dropped: Arc<AtomicBool>,
}

impl CachedDownloadService for ControlledCacheService {
    fn start(
        &mut self,
        _player: &dyn PlaybackBackend,
        _config: &Config,
        _destination: &Path,
        _thumbnail: Option<&url::Url>,
    ) -> Option<Box<dyn CachedDownloadJob>> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        Some(Box::new(ControlledCacheJob {
            result: Arc::clone(&self.result),
            dropped: Arc::clone(&self.dropped),
        }))
    }
}

impl CachedDownloadJob for ControlledCacheJob {
    fn poll(&mut self) -> Option<Result<Box<dyn CachedDownloadArtifact>, ()>> {
        self.result.lock().unwrap().take()
    }
}

impl Drop for ControlledCacheJob {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

struct FixtureArtifact;

impl CachedDownloadArtifact for FixtureArtifact {
    fn publish(
        self: Box<Self>,
        destination: &Path,
        _title: &str,
        _id: &str,
    ) -> Result<CachedDownloadPublished, String> {
        let path = destination.join("fixture.opus");
        std::fs::write(&path, b"verified cached audio").unwrap();
        Ok(CachedDownloadPublished {
            path,
            thumbnail_missing: false,
        })
    }
}

/// A playing YouTube item and two independently controlled completion channels.
fn cached_controller() -> (
    AppController,
    Arc<Mutex<Vec<DownloadRequest>>>,
    tempfile::TempDir,
    CacheResult,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
) {
    let directory = crate::test_support::canonical_tempdir("controlled cached download");
    let process = MockRunningDownload {
        progress: Some(Cursor::new(Vec::new())),
        errors: Some(Cursor::new(Vec::new())),
        exits: VecDeque::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (mut controller, requests, _) =
        controller_with_mock_download(Config::for_dir(directory.path().join("config")), process);
    let (mut playback_controller, _) = controller_with_mock_statuses([]);
    controller.playback_factory = playback_controller.playback_factory.take();
    let item = controller.selected_queue_item().unwrap();
    controller.play_queue_item(item, false);
    controller.playback_phase = PlaybackPhase::Playing;
    controller.view.playback.idle = false;
    let result = Arc::new(Mutex::new(None));
    let dropped = Arc::new(AtomicBool::new(false));
    let starts = Arc::new(AtomicUsize::new(0));
    controller.cached_download_service = Box::new(ControlledCacheService {
        result: Arc::clone(&result),
        dropped: Arc::clone(&dropped),
        starts: Arc::clone(&starts),
    });
    (controller, requests, directory, result, dropped, starts)
}

#[test]
fn verified_cache_hit_is_a_completed_download_without_a_network_process() {
    let (mut controller, requests, _directory, result, dropped, starts) = cached_controller();
    controller.dispatch(UiAction::Download);
    *result.lock().unwrap() = Some(Ok(Box::new(FixtureArtifact)));
    controller.poll_download_at(Instant::now());
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(starts.load(Ordering::Relaxed), 1);
    assert!(dropped.load(Ordering::Acquire));
    let download = controller.view.download.as_ref().unwrap();
    assert!(!download.active);
    assert_eq!(download.completed_files, 1);
    assert!(Path::new(download.completed_path.as_ref().unwrap()).is_file());
    assert!(
        controller
            .view
            .status_line
            .starts_with("Saved from playback cache:")
    );
    assert!(controller.download_completion_notice_deadline.is_some());
    controller.shutdown();
}

#[test]
fn cache_miss_falls_back_once_using_the_frozen_source() {
    let (mut controller, requests, _directory, result, _, starts) = cached_controller();
    let original = controller.selected_queue_item().unwrap().media.webpage_url;
    controller.dispatch(UiAction::Download);
    controller.youtube_results.clear();
    *result.lock().unwrap() = Some(Err(()));
    controller.poll_download_at(Instant::now());
    controller.poll_download_at(Instant::now());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].source_url, original);
    assert_eq!(starts.load(Ordering::Relaxed), 1);
    assert!(controller.pending_cached_download.is_none());
    drop(requests);
    controller.shutdown();
}

#[test]
fn cancellation_discards_a_ready_cache_result_and_never_falls_back() {
    let (mut controller, requests, _directory, result, dropped, _) = cached_controller();
    controller.dispatch(UiAction::Download);
    *result.lock().unwrap() = Some(Ok(Box::new(FixtureArtifact)));
    let now = Instant::now();
    controller.cancel_active_download_at(now);
    controller.poll_download_at(now + Duration::from_secs(1));
    assert!(requests.lock().unwrap().is_empty());
    assert!(dropped.load(Ordering::Acquire));
    assert!(
        !controller
            .config
            .downloads_dir()
            .join("fixture.opus")
            .exists()
    );
    assert!(!controller.view.download.as_ref().unwrap().active);
    assert_eq!(
        controller.download_cancellation_notice_deadline,
        Some(now + DOWNLOAD_CANCELLATION_NOTICE_DURATION)
    );
    controller.poll_download_at(now + DOWNLOAD_CANCELLATION_NOTICE_DURATION);
    assert!(controller.view.download.is_none());
    controller.shutdown();
}

#[test]
fn pending_cache_occupies_the_manual_and_automatic_download_slot() {
    let (mut controller, requests, _directory, _, _, starts) = cached_controller();
    controller.dispatch(UiAction::Download);
    controller.dispatch(UiAction::Download);
    controller
        .automatic_download_queue
        .push_back(AutomaticDownloadJob {
            channel_id: "UCfixture".to_owned(),
            channel_name: "Fixture".to_owned(),
            source_url: url::Url::parse("https://www.youtube.com/playlist?list=UUfixture").unwrap(),
        });
    controller.start_next_automatic_download();
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(controller.automatic_download_queue.len(), 1);
    assert_eq!(starts.load(Ordering::Relaxed), 1);
    controller.shutdown();
}

#[test]
fn shutdown_cancels_cache_preparation_without_starting_a_download() {
    let (mut controller, requests, _directory, _, dropped, _) = cached_controller();
    controller.dispatch(UiAction::Download);
    controller.shutdown();
    assert!(dropped.load(Ordering::Acquire));
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn original_video_incompatible_codec_and_other_playing_items_skip_cache_lookup() {
    for format in [
        DownloadFormat::ExactFile,
        DownloadFormat::BestVideo,
        DownloadFormat::OriginalBestAudio,
        DownloadFormat::TranscodeToOpus,
    ] {
        let (mut controller, requests, _directory, _, _, starts) = cached_controller();
        let item = controller.selected_queue_item().unwrap();
        controller.launch_manual_download(item.clone(), item.media.webpage_url.clone(), format);
        assert_eq!(starts.load(Ordering::Relaxed), 0);
        assert_eq!(requests.lock().unwrap().len(), 1);
        controller.shutdown();
    }
    for changed_id in [true, false] {
        let (mut controller, requests, _directory, _, _, starts) = cached_controller();
        if changed_id {
            controller.current_media = Some(MediaId::new(SourceKind::YouTube, "different"));
        } else {
            controller.view.playback.live = true;
        }
        controller.dispatch(UiAction::Download);
        assert_eq!(starts.load(Ordering::Relaxed), 0);
        assert_eq!(requests.lock().unwrap().len(), 1);
        controller.shutdown();
    }
}

#[test]
fn a_changed_playing_item_cannot_publish_a_ready_old_cache_artifact() {
    let (mut controller, requests, _directory, result, _, _) = cached_controller();
    controller.dispatch(UiAction::Download);
    *result.lock().unwrap() = Some(Ok(Box::new(FixtureArtifact)));
    controller.current_media = Some(MediaId::new(SourceKind::YouTube, "different"));
    controller.poll_download_at(Instant::now());
    assert!(
        !controller
            .config
            .downloads_dir()
            .join("fixture.opus")
            .exists()
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    controller.shutdown();
}
