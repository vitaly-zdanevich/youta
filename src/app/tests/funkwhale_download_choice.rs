//! Captured Funkwhale format decisions use bounded local HTTP fixtures only.

use super::*;
use crate::download_queue::{DownloadQueueState, DownloadSource, QueuedDownloadFormat};
use std::io::{Read, Write};
use std::net::TcpListener;

/// Keeps cancellation-bound admission deterministic across parallel test runners.
static CHOOSER_TESTS: Mutex<()> = Mutex::new(());

/// Serves advertised metadata separately from audio, with an optional held metadata response.
struct Pod {
    page: url::Url,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    audio: Vec<u8>,
}

impl Pod {
    fn new(extension: &'static str, hold: bool, fail: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/pod/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(!hold));
        let observed = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let released = Arc::clone(&release);
        let audio = if extension == "opus" {
            let mut bytes = b"OggS".to_vec();
            bytes.resize(26, 0);
            bytes.extend_from_slice(b"\x01\x13OpusHead");
            bytes
        } else {
            b"ID3\x03\0unchanged original audio and tags".to_vec()
        };
        let body = audio.clone();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stopping.load(AtomicOrdering::Acquire) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                crate::test_support::configure_http_fixture_stream(&socket, Duration::from_secs(2));
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    let mut byte = [0];
                    if socket.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let target = request.split_whitespace().nth(1).unwrap().to_owned();
                observed.lock().unwrap().push(target.clone());
                let metadata = target == "/pod/api/v1/tracks/27/";
                while metadata
                    && !released.load(AtomicOrdering::Acquire)
                    && !stopping.load(AtomicOrdering::Acquire)
                {
                    thread::sleep(Duration::from_millis(2));
                }
                let (status, content_type, bytes) = if metadata && fail {
                    (
                        "503 Service Unavailable",
                        "application/json",
                        b"{}".to_vec(),
                    )
                } else if metadata {
                    ("200 OK", "application/json", serde_json::to_vec(&serde_json::json!({
                        "id": 27, "title": "Captured track", "listen_url": "/pod/api/v1/listen/track/?temporary=private",
                        "uploads": [{"uuid": "12345678-1234-1234-1234-123456789abc", "extension": extension,
                            "mimetype": if extension == "opus" { "audio/opus" } else { "audio/mpeg" }, "size": body.len()}]
                    })).unwrap())
                } else {
                    assert!(
                        target.starts_with("/pod/api/v1/listen/track/?"),
                        "no HTML page or unrelated pod: {target}"
                    );
                    (
                        "200 OK",
                        if extension == "opus" {
                            "audio/opus"
                        } else {
                            "audio/mpeg"
                        },
                        body.clone(),
                    )
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = socket.write_all(&bytes);
            }
        });
        Self {
            page: url::Url::parse(&format!("{base}library/tracks/27")).unwrap(),
            requests,
            stop,
            release,
            thread: Some(thread),
            audio,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Pod {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::Release);
        self.release.store(true, AtomicOrdering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Captures a normal queue entry while leaving current selection on another provider.
fn enqueue(controller: &mut AppController, pod: &Pod) {
    controller.manual_downloads.marks.push(DownloadSource {
        media_id: MediaId::new(SourceKind::Funkwhale, pod.page.as_str()),
        kind: MediaKind::Audio,
        title: "Captured track".to_owned(),
        creator: None,
        webpage_url: pod.page.clone(),
        download_url: pod.page.clone(),
        duration_seconds: Some(123),
    });
    controller.enqueue_selected_manual_download();
}

/// Drains only the requested metadata or native transfer, never starts a catalogue search.
fn until(controller: &mut AppController, ready: impl Fn(&AppController) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready(controller) {
        assert!(Instant::now() < deadline, "Funkwhale chooser timed out");
        controller.poll_manual_download_choice();
        controller.poll_download_at(Instant::now());
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn non_opus_funkwhale_download_asks_before_fetching_audio_or_changing_preferences() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pod = Pod::new("mp3", false, false);
    let (mut controller, requests, _directory) = choice_controller();
    enqueue(&mut controller, &pod);
    assert!(
        controller.view.download_choice_popup.is_some(),
        "metadata lookup belongs to a loading chooser"
    );
    until(&mut controller, |app| {
        app.view
            .download_choice_popup
            .as_ref()
            .is_some_and(|popup| popup.options.len() == 2)
    });
    let popup = controller.view.download_choice_popup.as_ref().unwrap();
    assert!(popup.options[0].contains("Original"));
    assert!(popup.options[1].contains("Opus"));
    assert_eq!(controller.config.subscriptions.audio_format, "opus");
    assert_eq!(
        controller.store.download_queue().unwrap().entries[0].format,
        None
    );
    assert_eq!(pod.requests(), vec!["/pod/api/v1/tracks/27/"]);
    assert!(requests.lock().unwrap().is_empty());
    controller.dismiss_download_choice();
}

#[test]
fn choosing_original_keeps_captured_track_and_original_bytes_after_selection_changes() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pod = Pod::new("mp3", false, false);
    let (mut controller, _, _directory) = choice_controller();
    enqueue(&mut controller, &pod);
    until(&mut controller, |app| {
        app.view
            .download_choice_popup
            .as_ref()
            .is_some_and(|popup| popup.options.len() == 2)
    });
    let generation = controller
        .view
        .download_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    controller.config.providers.funkwhale_instance_url =
        Some(url::Url::parse("https://different-pod.invalid/").unwrap());
    controller.youtube_results.clear();
    controller.confirm_download_choice(generation.wrapping_add(1), Some(1));
    controller.confirm_download_choice(generation, Some(usize::MAX));
    assert_eq!(pod.requests().len(), 1);
    controller.confirm_download_choice(generation, Some(0));
    until(&mut controller, |app| {
        app.store.download_queue().unwrap().entries[0].state == DownloadQueueState::Completed
    });
    let queue = controller.store.download_queue().unwrap();
    assert_eq!(
        queue.entries[0].format,
        Some(QueuedDownloadFormat::OriginalBestAudio)
    );
    assert_eq!(queue.entries[0].source.webpage_url, pod.page);
    let output = controller
        .config
        .downloads_dir()
        .join(queue.entries[0].completed_path.as_ref().unwrap());
    assert_eq!(std::fs::read(output).unwrap(), pod.audio);
    controller.confirm_download_choice(generation, Some(1));
    assert_eq!(
        pod.requests().len(),
        3,
        "one inspection and a fresh native metadata/audio transfer"
    );
    assert_eq!(controller.config.subscriptions.audio_format, "opus");
}

#[test]
fn cancelling_a_held_funkwhale_metadata_lookup_cannot_start_audio_later() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pod = Pod::new("mp3", true, false);
    let (mut controller, _, _directory) = choice_controller();
    enqueue(&mut controller, &pod);
    until(&mut controller, |_| pod.requests().len() == 1);
    let started = Instant::now();
    controller.dismiss_download_choice();
    assert!(started.elapsed() < Duration::from_secs(1));
    pod.release.store(true, AtomicOrdering::Release);
    for _ in 0..10 {
        controller.poll_manual_download_choice();
        thread::yield_now();
    }
    assert!(controller.view.download_choice_popup.is_none());
    assert_eq!(
        controller.store.download_queue().unwrap().entries[0].state,
        DownloadQueueState::Cancelled
    );
    assert_eq!(pod.requests().len(), 1);
}

#[test]
fn advertised_opus_keeps_copy_policy_without_an_unnecessary_format_question() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pod = Pod::new("opus", false, false);
    let (mut controller, _, _directory) = choice_controller();
    enqueue(&mut controller, &pod);
    until(&mut controller, |app| {
        app.store.download_queue().unwrap().entries[0].state == DownloadQueueState::Completed
    });
    assert!(controller.view.download_choice_popup.is_none());
    assert_eq!(
        controller.store.download_queue().unwrap().entries[0].format,
        Some(QueuedDownloadFormat::OpusWithoutTranscoding)
    );
    assert_eq!(pod.requests().len(), 3);
}

#[test]
fn selecting_opus_conversion_is_explicit_per_item_and_does_not_change_the_default() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pod = Pod::new("mp3", false, false);
    let (mut controller, _, _directory) = choice_controller();
    enqueue(&mut controller, &pod);
    until(&mut controller, |app| {
        app.view
            .download_choice_popup
            .as_ref()
            .is_some_and(|popup| popup.options.len() == 2)
    });
    select_current_choice(&mut controller, 1);
    assert_eq!(
        controller.store.download_queue().unwrap().entries[0].format,
        Some(QueuedDownloadFormat::TranscodeToOpus)
    );
    assert_eq!(controller.config.subscriptions.audio_format, "opus");
    controller.cancel_active_download();
}

#[test]
fn metadata_failure_is_concise_without_an_audio_request_or_implicit_conversion() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pod = Pod::new("mp3", false, true);
    let (mut controller, _, _directory) = choice_controller();
    enqueue(&mut controller, &pod);
    until(&mut controller, |app| app.view.error_popup.is_some());
    assert!(controller.view.download_choice_popup.is_none());
    assert_eq!(
        controller.store.download_queue().unwrap().entries[0].format,
        None
    );
    assert!(!controller.view.error_popup.as_ref().unwrap().reportable);
    assert_eq!(pod.requests().len(), 1);
}

#[test]
fn explicitly_configured_original_and_transcode_policies_skip_format_inspection() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (configured, expected) in [
        ("original", QueuedDownloadFormat::OriginalBestAudio),
        ("transcode-opus", QueuedDownloadFormat::TranscodeToOpus),
    ] {
        let pod = Pod::new("mp3", true, false);
        let (mut controller, _, _directory) = choice_controller();
        controller.config.subscriptions.audio_format = configured.to_owned();
        enqueue(&mut controller, &pod);
        until(&mut controller, |_| pod.requests().len() == 1);
        assert!(controller.view.download_choice_popup.is_none());
        assert_eq!(
            controller.store.download_queue().unwrap().entries[0].format,
            Some(expected)
        );
        assert_eq!(controller.config.subscriptions.audio_format, configured);
        controller.cancel_active_download();
        pod.release.store(true, AtomicOrdering::Release);
    }
}

#[test]
fn cancelled_format_metadata_cannot_retarget_or_reopen_a_newer_download() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let old = Pod::new("mp3", true, false);
    let new = Pod::new("opus", false, false);
    let (mut controller, _, _directory) = choice_controller();
    enqueue(&mut controller, &old);
    until(&mut controller, |_| old.requests().len() == 1);
    controller.dismiss_download_choice();
    enqueue(&mut controller, &new);
    old.release.store(true, AtomicOrdering::Release);
    until(&mut controller, |app| {
        app.store.download_queue().unwrap().entries[1].state == DownloadQueueState::Completed
    });
    let queue = controller.store.download_queue().unwrap();
    assert_eq!(queue.entries[0].state, DownloadQueueState::Cancelled);
    assert_eq!(queue.entries[1].source.webpage_url, new.page);
    assert_eq!(
        queue.entries[1].format,
        Some(QueuedDownloadFormat::OpusWithoutTranscoding)
    );
    assert!(controller.view.download_choice_popup.is_none());
    assert!(controller.view.error_popup.is_none());
    assert_eq!(
        old.requests().len(),
        1,
        "cancelled metadata never downloads audio"
    );
}

#[test]
fn notified_format_result_cannot_start_audio_during_quit_and_can_resume_a_cancelled_quit() {
    let _native = crate::app::funkwhale_download::test_guard();
    let _serial = CHOOSER_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    struct WakeCount(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::AcqRel);
        }
    }
    let pod = Pod::new("mp3", true, false);
    let (mut controller, _, _directory) = choice_controller();
    let wakes = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
    controller
        .worker_notifier
        .set_waker(Some(std::task::Waker::from(wakes.clone())));
    let baseline = wakes.0.load(AtomicOrdering::Acquire);
    enqueue(&mut controller, &pod);
    until(&mut controller, |_| pod.requests().len() == 1);
    controller.view.quitting = true;
    pod.release.store(true, AtomicOrdering::Release);
    let deadline = Instant::now() + Duration::from_secs(3);
    while wakes.0.load(AtomicOrdering::Acquire) == baseline {
        assert!(
            Instant::now() < deadline,
            "metadata must wake the frontend after publication"
        );
        thread::yield_now();
    }
    controller.poll_manual_download_choice();
    assert!(
        controller
            .view
            .download_choice_popup
            .as_ref()
            .unwrap()
            .options
            .is_empty()
    );
    assert_eq!(
        controller.store.download_queue().unwrap().entries[0].format,
        None
    );
    assert_eq!(pod.requests().len(), 1);
    controller.view.quitting = false;
    until(&mut controller, |app| {
        app.view
            .download_choice_popup
            .as_ref()
            .is_some_and(|popup| popup.options.len() == 2)
    });
    controller.dismiss_download_choice();
}
