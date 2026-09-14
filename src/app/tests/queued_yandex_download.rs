//! Native download ownership regressions using captured batches and local workers.

use super::*;

/// Creates an isolated controller without starting a provider request.
fn native_download_controller() -> (AppController, tempfile::TempDir) {
    let directory = crate::test_support::canonical_tempdir("queued Yandex download");
    let config = Config::for_dir(directory.path().join("youta"));
    let store = StateStore::open_in_memory().expect("in-memory state");
    (AppController::new(config, store, None, None), directory)
}

/// An old worker must not join or discard the current native worker handle.
#[test]
fn stale_native_completion_preserves_current_worker_and_cancellation() {
    let (mut controller, _directory) = native_download_controller();
    controller.yandex_music_download_generation = 2;
    controller.yandex_music_download_thread = Some(thread::spawn(|| {}));
    let cancellation = Arc::new(AtomicBool::new(false));
    controller.yandex_music_download_cancel = Some(Arc::clone(&cancellation));
    controller
        .yandex_music_media_job_sender
        .send(YandexMusicMediaJobResponse::DownloadFinished {
            generation: 1,
            batch_title: "Superseded download".to_owned(),
            completed_paths: Vec::new(),
            failures: vec!["superseded fixture".to_owned()],
        })
        .expect("stale native completion");

    controller.drain_yandex_music_media_job_responses();

    assert!(controller.yandex_music_download_thread.is_some());
    assert!(
        controller
            .yandex_music_download_cancel
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &cancellation))
    );
    assert!(!cancellation.load(AtomicOrdering::Acquire));
    assert!(controller.view.error_popup.is_none());
    controller.shutdown();
}

/// Captures only stable native identity and public metadata for resumed work.
fn native_source() -> crate::download_queue::DownloadSource {
    let track = yandex_music_track_fixture();
    crate::download_queue::DownloadSource {
        media_id: MediaId::new(SourceKind::YandexMusic, &track.id),
        kind: MediaKind::Audio,
        title: track.title,
        creator: Some(yandex_music_artist_names(&track.artists)),
        webpage_url: track.webpage_url.clone(),
        download_url: track.webpage_url,
        duration_seconds: Some(183),
    }
}

/// Installs one running native attempt and one pending sibling without workers.
fn install_native_queue_owner(controller: &mut AppController) {
    use crate::download_queue::{
        DownloadQueue, DownloadQueueEntry, DownloadQueueState, QueuedDownloadFormat,
    };
    let entry = |id, state, attempt| DownloadQueueEntry {
        id,
        source: native_source(),
        format: Some(QueuedDownloadFormat::YandexOriginal),
        state,
        attempt,
        completed_path: None,
    };
    let queue = DownloadQueue {
        next_id: 3,
        entries: vec![
            entry(1, DownloadQueueState::Running, 1),
            entry(2, DownloadQueueState::Queued, 0),
        ],
    };
    controller.store.save_download_queue(&queue).unwrap();
    controller.manual_downloads.queue = queue;
    controller.manual_downloads.active = Some((1, 1));
    controller.manual_downloads.yandex_generation = Some(4);
    controller.yandex_music_download_generation = 4;
}

/// Native terminal results belong to the captured queue generation, including failures.
#[test]
fn native_terminal_response_finishes_only_its_matching_queue_attempt() {
    use crate::download_queue::DownloadQueueState;
    for (matching_owner, success) in [(true, true), (true, false), (false, true)] {
        let (mut controller, _directory) = native_download_controller();
        controller.config.ensure_directories().unwrap();
        let output = controller.config.downloads_dir().join("fixture.flac");
        std::fs::write(&output, b"fixture original media").unwrap();
        install_native_queue_owner(&mut controller);
        if !matching_owner {
            controller.manual_downloads.yandex_generation = Some(3);
        }
        controller
            .yandex_music_media_job_sender
            .send(YandexMusicMediaJobResponse::DownloadFinished {
                generation: 4,
                batch_title: "Native fixture".to_owned(),
                completed_paths: if success { vec![output] } else { Vec::new() },
                failures: if success {
                    Vec::new()
                } else {
                    vec!["fixture failure".to_owned()]
                },
            })
            .unwrap();
        controller.drain_yandex_music_media_job_responses();
        let saved = controller.store.download_queue().unwrap();
        assert_eq!(
            saved.entries[0].state,
            if !matching_owner {
                DownloadQueueState::Running
            } else if success {
                DownloadQueueState::Completed
            } else {
                DownloadQueueState::Failed
            }
        );
        assert_eq!(saved.entries[1].state, DownloadQueueState::Queued);
        assert_eq!(
            controller.manual_downloads.active,
            (!matching_owner).then_some((1, 1))
        );
        controller.shutdown();
    }
}

/// Cancelling one active native attempt must preserve its queued siblings.
#[test]
fn native_cancellation_stops_only_the_matching_durable_attempt() {
    use crate::download_queue::DownloadQueueState;
    let (mut controller, _directory) = native_download_controller();
    install_native_queue_owner(&mut controller);
    controller.yandex_music_download_thread = Some(thread::spawn(|| {}));
    controller.yandex_music_download_cancel = Some(Arc::new(AtomicBool::new(false)));
    assert!(controller.cancel_yandex_music_download());
    assert!(controller.manual_downloads.active.is_none());
    assert!(controller.manual_downloads.yandex_generation.is_none());
    let saved = controller.store.download_queue().unwrap();
    assert_eq!(saved.entries[0].state, DownloadQueueState::Cancelled);
    assert_eq!(saved.entries[1].state, DownloadQueueState::Queued);
    controller.shutdown();
}

/// Missing credentials must fail startup rather than leave durable work running.
#[test]
fn queued_native_download_without_token_returns_error_before_launch() {
    let (mut controller, _directory) = native_download_controller();
    let error = controller
        .start_queued_yandex_download(&native_source())
        .unwrap_err();
    assert!(error.contains("OAuth token"));
    assert!(controller.view.yandex_music_setup_popup.is_some());
    assert!(controller.yandex_music_download_thread.is_none());
    assert!(controller.manual_downloads.yandex_generation.is_none());
    controller.shutdown();
}

/// Filesystem preparation errors likewise require no future worker response.
#[test]
fn queued_native_download_directory_failure_returns_error_without_worker() {
    let (mut controller, directory) = native_download_controller();
    let blocked = directory.path().join("not-a-directory");
    std::fs::write(&blocked, b"fixture file").unwrap();
    controller.config = Config::for_dir(blocked);
    controller.config.providers.yandex_music_token = Some("fixture-token".to_owned());
    let error = controller
        .start_queued_yandex_download(&native_source())
        .unwrap_err();
    assert!(error.contains("download directory"));
    assert!(controller.yandex_music_download_thread.is_none());
    assert!(controller.manual_downloads.yandex_generation.is_none());
    controller.shutdown();
}

/// Native replay reconstructs provider metadata without a signed media URL.
#[test]
fn queued_native_download_captures_stable_identity_and_owns_started_generation() {
    let (mut controller, _directory) = native_download_controller();
    controller.config.providers.yandex_music_token = Some("fixture-token".to_owned());
    controller.manual_downloads.active = Some((7, 8));
    let (capture, batches) = bounded(1);
    controller.yandex_music_download_batch_capture = Some(capture);
    let source = native_source();
    controller.start_queued_yandex_download(&source).unwrap();
    let (_, items) = batches.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(items.len(), 1);
    let track = &items[0].track;
    assert_eq!(track.id, source.media_id.external_id);
    assert_eq!(track.title, source.title);
    assert_eq!(track.webpage_url, source.webpage_url);
    assert_eq!(track.duration_ms, Some(183_000));
    assert!(track.album.is_none());
    assert!(track.artwork_url.is_none());
    assert_eq!(
        items[0].file_stem,
        "First Artist, Second Artist — Fixture Track"
    );
    assert_eq!(
        controller.manual_downloads.yandex_generation,
        Some(controller.yandex_music_download_generation)
    );
    controller.shutdown();
}

/// A stopped native worker must neither block input nor cancel another queue owner.
#[test]
fn native_cancel_retains_stopping_worker_and_preserves_unrelated_queue_owner() {
    let (mut controller, _directory) = native_download_controller();
    let (release, wait) = bounded::<()>(1);
    controller.yandex_music_download_thread = Some(thread::spawn(move || {
        let _ = wait.recv_timeout(Duration::from_secs(1));
    }));
    let cancellation = Arc::new(AtomicBool::new(false));
    controller.yandex_music_download_cancel = Some(Arc::clone(&cancellation));
    controller.yandex_music_download_generation = 4;
    controller.manual_downloads.active = Some((7, 8));
    controller.manual_downloads.yandex_generation = Some(3);
    assert!(controller.cancel_yandex_music_download());
    assert!(cancellation.load(AtomicOrdering::Acquire));
    assert_eq!(controller.yandex_music_download_generation, 5);
    assert!(controller.yandex_music_download_thread.is_some());
    assert_eq!(controller.manual_downloads.active, Some((7, 8)));
    assert_eq!(controller.manual_downloads.yandex_generation, Some(3));
    release.send(()).unwrap();
    controller.shutdown();
}
