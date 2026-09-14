//! Manual queue regression tests use supervised fake downloads, never a network.

use super::*;
use crate::download_queue::DownloadQueueState;

/// A never-completed child keeps later queue entries pending for inspection.
fn pending_process() -> MockRunningDownload {
    MockRunningDownload {
        progress: Some(Cursor::new(Vec::new())),
        errors: Some(Cursor::new(Vec::new())),
        exits: VecDeque::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn marked_downloads_capture_each_identity_and_persist_in_mark_order() {
    let temporary = crate::test_support::canonical_tempdir("marked download queue");
    let config = Config::for_dir(temporary.path().join("config"));
    let (mut controller, requests, _) = controller_with_mock_download(config, pending_process());
    let first = fixture_download_video();
    let mut second = first.clone();
    second.video_id = "abcdefghijk".to_owned();
    second.title = "Second captured title".to_owned();
    controller.youtube_results = vec![SearchItem::Video(first), SearchItem::Video(second)];
    controller.refresh_youtube_rows();
    controller.dispatch(UiAction::ToggleDownloadMark);
    controller.dispatch(UiAction::SelectRow(1));
    controller.dispatch(UiAction::ToggleDownloadMark);
    assert!(
        controller.view.rows.iter().all(|row| row.download_marked),
        "{}",
        controller.view.status_line
    );
    controller.dispatch(UiAction::Download);

    let saved = controller.store.download_queue().unwrap();
    assert_eq!(saved.entries.len(), 2);
    assert_eq!(saved.entries[0].source.media_id.external_id, "dQw4w9WgXcQ");
    assert_eq!(saved.entries[1].source.media_id.external_id, "abcdefghijk");
    assert_eq!(saved.entries[1].source.title, "Second captured title");
    assert_eq!(saved.entries[0].state, DownloadQueueState::Running);
    assert_eq!(saved.entries[1].state, DownloadQueueState::Queued);
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(controller.view.rows.iter().all(|row| !row.download_marked));
}

#[test]
fn cancellation_preserves_other_jobs_and_stale_completion_is_ignored() {
    let temporary = crate::test_support::canonical_tempdir("cancel queued download");
    let config = Config::for_dir(temporary.path().join("config"));
    let (mut controller, _, cancelled) = controller_with_mock_download(config, pending_process());
    controller.dispatch(UiAction::Download);
    let owner = controller.manual_downloads.active.unwrap();
    let mut second = fixture_download_video();
    second.video_id = "abcdefghijk".to_owned();
    controller.youtube_results = vec![SearchItem::Video(second)];
    controller.refresh_youtube_rows();
    controller.dispatch(UiAction::Download);
    controller.dispatch(UiAction::CancelQueuedDownload(owner.0));
    let saved = controller.store.download_queue().unwrap();
    assert!(cancelled.load(AtomicOrdering::Acquire));
    assert_eq!(saved.entries[0].state, DownloadQueueState::Cancelled);
    assert_eq!(saved.entries[1].state, DownloadQueueState::Queued);
    controller.finish_manual_download_for(owner, Err(()));
    assert_eq!(controller.store.download_queue().unwrap(), saved);
}

#[test]
fn completed_indicator_requires_a_still_present_file_and_survives_restart() {
    let temporary = crate::test_support::canonical_tempdir("downloaded indicator");
    let config = Config::for_dir(temporary.path().join("config"));
    let (mut controller, _, _) = controller_with_mock_download(config.clone(), pending_process());
    // Use the actual file backend to prove this is not only an in-memory mark.
    controller.store = StateStore::open(&config).unwrap();
    controller.dispatch(UiAction::Download);
    let owner = controller.manual_downloads.active.unwrap();
    let path = config.downloads_dir().join("completed.opus");
    std::fs::write(&path, b"fixture original audio").unwrap();
    controller.finish_manual_download_for(owner, Ok(path.clone()));
    controller.refresh_download_markers(true);
    assert!(controller.view.rows[0].downloaded);
    controller.shutdown();
    drop(controller);

    let store = StateStore::open(&config).unwrap();
    let mut restored = AppController::new(config, store, None, None);
    restored.youtube_results = vec![SearchItem::Video(fixture_download_video())];
    restored.refresh_youtube_rows();
    restored.refresh_download_markers(true);
    assert!(restored.view.rows[0].downloaded);
    std::fs::remove_file(path).unwrap();
    restored.refresh_download_markers(true);
    assert!(!restored.view.rows[0].downloaded);
}

#[test]
fn failed_queue_save_does_not_start_a_child_or_discard_marks() {
    let temporary = crate::test_support::canonical_tempdir("queue save conflict");
    let config = Config::for_dir(temporary.path().join("config"));
    let (mut controller, requests, _) =
        controller_with_mock_download(config.clone(), pending_process());
    controller.store = StateStore::open(&config).unwrap();
    controller.dispatch(UiAction::ToggleDownloadMark);
    // A conflicting writer changes the authoritative document after it was read.
    let document = config.state_dir().join("downloads.toml");
    let original = std::fs::read_to_string(&document).unwrap();
    std::fs::write(&document, format!("{original}\n# External edit\n")).unwrap();
    controller.dispatch(UiAction::Download);
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(controller.manual_downloads.marks.len(), 1);
    assert!(controller.manual_downloads.blocked);
}

/// Each launch owns a distinct fake process, allowing failure and retry ordering assertions.
struct SequenceLauncher {
    requests: Arc<Mutex<Vec<DownloadRequest>>>,
    processes: VecDeque<MockRunningDownload>,
}

impl DownloadLauncher for SequenceLauncher {
    fn start(&mut self, request: &DownloadRequest) -> Result<Box<dyn RunningDownload>, String> {
        self.requests.lock().unwrap().push(request.clone());
        self.processes
            .pop_front()
            .map(|process| Box::new(process) as Box<dyn RunningDownload>)
            .ok_or_else(|| "No fake process remains".to_owned())
    }
}

#[test]
fn failed_job_advances_and_retry_keeps_its_confirmed_format() {
    let temporary = crate::test_support::canonical_tempdir("queue failure and retry");
    let config = Config::for_dir(temporary.path().join("config"));
    let (mut controller, requests, _) = controller_with_mock_download(config, pending_process());
    let mut failure = pending_process();
    failure.exits.push_back(Ok(Some(DownloadExit {
        success: false,
        description: "fixture failure".to_owned(),
    })));
    controller.download_launcher = Box::new(SequenceLauncher {
        requests: requests.clone(),
        processes: VecDeque::from([failure, pending_process(), pending_process()]),
    });
    controller.dispatch(UiAction::Download);
    let first = controller.manual_downloads.active.unwrap();
    let mut second = fixture_download_video();
    second.video_id = "abcdefghijk".to_owned();
    controller.youtube_results = vec![SearchItem::Video(second)];
    controller.refresh_youtube_rows();
    controller.dispatch(UiAction::Download);
    controller.poll_download_at(Instant::now());
    assert_eq!(
        controller.manual_downloads.queue.entries[0].state,
        DownloadQueueState::Failed
    );
    controller.poll_manual_download_queue();
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(
        controller.manual_downloads.queue.entries[1].state,
        DownloadQueueState::Running
    );

    let second_id = controller.manual_downloads.queue.entries[1].id;
    controller.cancel_queued_download(second_id);
    controller.config.downloads.mode = crate::config::DownloadMode::Video;
    controller.retry_queued_download(first.0);
    assert_eq!(
        requests.lock().unwrap()[2].format,
        DownloadFormat::AudioOnlyWithoutReencoding
    );
    assert!(controller.manual_downloads.active.unwrap().1 > first.1);
    let before = controller.store.download_queue().unwrap();
    controller.finish_manual_download_for(first, Err(()));
    assert_eq!(controller.store.download_queue().unwrap(), before);
}

#[test]
fn interrupted_work_restores_without_starting_a_process_in_the_constructor() {
    let temporary = crate::test_support::canonical_tempdir("queue restart");
    let config = Config::for_dir(temporary.path().join("config"));
    let (mut controller, _, _) = controller_with_mock_download(config.clone(), pending_process());
    controller.store = StateStore::open(&config).unwrap();
    controller.dispatch(UiAction::Download);
    let saved = controller.store.download_queue().unwrap();
    let first = controller.manual_downloads.active.unwrap();
    controller.shutdown();
    drop(controller);

    let store = StateStore::open(&config).unwrap();
    // Emulate the state left by a crash before orderly shutdown could requeue it.
    store.save_download_queue(&saved).unwrap();
    let mut restored = AppController::new(config, store, None, None);
    assert!(restored.active_download.is_none());
    assert!(restored.manual_downloads.active.is_none());
    assert_eq!(
        restored.manual_downloads.queue.entries[0].state,
        DownloadQueueState::Queued
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    restored.download_launcher = Box::new(SequenceLauncher {
        requests: requests.clone(),
        processes: VecDeque::from([pending_process()]),
    });
    restored.poll_manual_download_queue();
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        requests.lock().unwrap()[0].format,
        DownloadFormat::AudioOnlyWithoutReencoding
    );
    assert!(restored.manual_downloads.active.unwrap().1 > first.1);
}

/// A controlled cache miss can become ready even after its UI owner is cancelled.
#[cfg(feature = "backend-mpv")]
struct QueuedCacheFixture {
    starts: Arc<AtomicUsize>,
    ready: Arc<AtomicBool>,
}

#[cfg(feature = "backend-mpv")]
impl crate::app::cached_download::CachedDownloadService for QueuedCacheFixture {
    fn start(
        &mut self,
        _player: &dyn PlaybackBackend,
        _config: &Config,
        _destination: &Path,
        _thumbnail: Option<&url::Url>,
    ) -> Option<Box<dyn crate::app::cached_download::CachedDownloadJob>> {
        self.starts.fetch_add(1, AtomicOrdering::Relaxed);
        Some(Box::new(QueuedCacheFixture {
            starts: Arc::clone(&self.starts),
            ready: Arc::clone(&self.ready),
        }))
    }
}

#[cfg(feature = "backend-mpv")]
impl crate::app::cached_download::CachedDownloadJob for QueuedCacheFixture {
    fn poll(
        &mut self,
    ) -> Option<Result<Box<dyn crate::app::cached_download::CachedDownloadArtifact>, ()>> {
        self.ready
            .swap(false, AtomicOrdering::AcqRel)
            .then_some(Err(()))
    }
}

/// Archive's occupied-copy-slot deferral uses this same provider-independent owner path.
#[cfg(feature = "backend-mpv")]
#[test]
fn deferred_cache_preserves_attempt_and_cancellation_never_resurrects_it() {
    use crate::download_queue::{
        DownloadQueue, DownloadQueueEntry, DownloadSource, QueuedDownloadFormat,
    };
    for cancel_before_resume in [true, false] {
        let directory = crate::test_support::canonical_tempdir("deferred queue cache owner");
        let config = Config::for_dir(directory.path().join("config"));
        let (mut controller, requests, _) =
            controller_with_mock_download(config, pending_process());
        let (mut playback_controller, _) = controller_with_mock_statuses([]);
        controller.playback_factory = playback_controller.playback_factory.take();
        let item = controller.selected_queue_item().unwrap();
        controller.play_queue_item(item.clone(), false);
        controller.playback_phase = PlaybackPhase::Playing;
        controller.view.playback.idle = false;
        let starts = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        controller.cached_download_service = Box::new(QueuedCacheFixture {
            starts: Arc::clone(&starts),
            ready: Arc::clone(&ready),
        });
        let queue = DownloadQueue {
            next_id: 2,
            entries: vec![DownloadQueueEntry {
                id: 1,
                source: DownloadSource {
                    media_id: item.media.id.clone(),
                    kind: item.media.kind,
                    title: item.media.title.clone(),
                    creator: item.media.creator.clone(),
                    webpage_url: item.media.webpage_url.clone(),
                    download_url: item.media.webpage_url.clone(),
                    duration_seconds: item.media.duration_seconds,
                },
                format: Some(QueuedDownloadFormat::AudioOnlyWithoutReencoding),
                state: DownloadQueueState::Running,
                attempt: 3,
                completed_path: None,
            }],
        };
        controller.store.save_download_queue(&queue).unwrap();
        controller.manual_downloads.queue = queue.clone();
        let owner = (1, 3);
        controller.manual_downloads.active = Some(owner);
        let request = DownloadRequest {
            source_url: item.media.webpage_url.clone(),
            destination: controller.config.downloads_dir(),
            format: DownloadFormat::AudioOnlyWithoutReencoding,
            scope: DownloadScope::SingleItem,
            playlist_start: None,
            skip_shorts: false,
            write_thumbnail: false,
            archive_path: None,
        };

        // Enter exactly the scheduler boundary used when an original-copy
        // worker's predecessor still owns the shared preparation permit.
        controller.defer_manual_download_cache(&item, &request);
        assert_eq!(controller.manual_downloads.active, Some(owner));
        assert!(requests.lock().unwrap().is_empty());
        if !cancel_before_resume {
            controller.poll_manual_download_queue();
            assert_eq!(controller.manual_downloads.active, Some(owner));
            assert_eq!(controller.store.download_queue().unwrap(), queue);
            assert!(controller.pending_cached_download.is_some());
            assert_eq!(starts.load(AtomicOrdering::Relaxed), 1);
            assert!(requests.lock().unwrap().is_empty());
        }

        controller.cancel_active_download_at(Instant::now());
        ready.store(true, AtomicOrdering::Release);
        for _ in 0..2 {
            controller.poll_cached_download(Instant::now());
            controller.poll_manual_download_queue();
        }
        assert!(controller.manual_downloads.active.is_none());
        assert!(controller.pending_cached_download.is_none());
        assert_eq!(
            controller.store.download_queue().unwrap().entries[0].state,
            DownloadQueueState::Cancelled
        );
        assert_eq!(
            starts.load(AtomicOrdering::Relaxed),
            usize::from(!cancel_before_resume)
        );
        assert!(
            requests.lock().unwrap().is_empty(),
            "a cancelled cache owner must never start a network fallback"
        );
        controller.shutdown();
    }
}
