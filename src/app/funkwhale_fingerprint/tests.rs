//! Offline ownership, bounded staging, and metadata-preservation regressions.

use super::*;
use std::io::Cursor;
use std::net::TcpListener;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const UPLOAD: &str = "17c770f0-1a57-40e9-85de-d2ca75b1d42d";

/// Builds one complete public-track descriptor without artist or owner assumptions.
fn raw_audio(listen: &str, uuid: &str, size: usize) -> serde_json::Value {
    serde_json::json!({
        "id": 7,
        "is_playable": true,
        "listen_url": listen,
        "uploads": [{"uuid": uuid, "size": size, "duration": 146, "listen_url": null}],
    })
}

/// Provides no live service access: only bounded local metadata and fake audio bytes.
struct Pod {
    base: url::Url,
    requests: Arc<Mutex<Vec<String>>>,
    revision: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Pod {
    fn new() -> Self {
        Self::with_audio_delay(Duration::ZERO)
    }

    /// Delays only encoded audio chunks, leaving metadata and connection setup fast.
    fn with_audio_delay(audio_delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = url::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let revision = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let seen = requests.clone();
        let changing = revision.clone();
        let stopping = stopped.clone();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stopping.load(Ordering::Relaxed) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("mock pod accept: {error}"),
                };
                crate::test_support::configure_http_fixture_stream(&socket, Duration::from_secs(1));
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    let mut byte = [0];
                    if socket.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(!request.to_ascii_lowercase().contains("cookie:"));
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
                let target = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                seen.lock().unwrap().push(target.clone());
                let (kind, bytes) = if target == "/api/v1/tracks/7/" {
                    let uuid = if changing.load(Ordering::Relaxed) == 0 {
                        UPLOAD
                    } else {
                        "27c770f0-1a57-40e9-85de-d2ca75b1d42d"
                    };
                    (
                        "application/json",
                        raw_audio("/audio", uuid, 11).to_string().into_bytes(),
                    )
                } else if target == "/audio" {
                    ("audio/flac", b"fLaCfixture".to_vec())
                } else {
                    panic!("unexpected mock request: {target}");
                };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
                if kind.starts_with("audio/") && !audio_delay.is_zero() {
                    for chunk in bytes.chunks(1) {
                        thread::sleep(audio_delay);
                        if socket.write_all(chunk).is_err() {
                            break;
                        }
                    }
                } else {
                    socket.write_all(&bytes).unwrap();
                }
            }
        });
        Self {
            base,
            requests,
            revision,
            stopped,
            thread: Some(thread),
        }
    }

    fn source(&self) -> url::Url {
        self.base.join("library/tracks/7").unwrap()
    }
}

impl Drop for Pod {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.expect("fingerprint pod fixture worker completed");
            }
        }
    }
}

/// A failed fixture worker must not abort the harness while another panic unwinds.
#[test]
fn fixture_drop_preserves_original_panic_when_worker_panics() {
    const CHILD: &str = "YOUTA_FUNKWHALE_FIXTURE_DROP_TEST_CHILD";
    const COMPLETED: &str = "fixture cleanup preserved original panic";
    if std::env::var_os(CHILD).is_none() {
        let directory = crate::test_support::canonical_tempdir("fixture panic subprocess");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
			.args([
				"--exact",
				"app::funkwhale_fingerprint::tests::fixture_drop_preserves_original_panic_when_worker_panics",
				"--nocapture",
			])
			.env(CHILD, "1")
			.env("RUST_BACKTRACE", "0")
			.current_dir(directory.path())
			.output()
			.expect("fixture panic subprocess");
        assert!(
            output.status.success(),
            "fixture cleanup must preserve the original panic: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(COMPLETED));
        return;
    }

    let result = std::panic::catch_unwind(|| {
        let _pod = Pod {
            base: url::Url::parse("http://127.0.0.1/").unwrap(),
            requests: Arc::new(Mutex::new(Vec::new())),
            revision: Arc::new(AtomicUsize::new(0)),
            stopped: Arc::new(AtomicBool::new(false)),
            thread: Some(thread::spawn(|| panic!("fixture worker panic"))),
        };
        panic!("original fixture panic");
    });
    assert_eq!(
        result.unwrap_err().downcast_ref::<&str>(),
        Some(&"original fixture panic")
    );
    println!("{COMPLETED}");
}

/// Records the actual private complete file, replacing both fpcalc and AcoustID.
struct Identifier {
    calls: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl AudioIdentifier for Identifier {
    fn identify(
        &mut self,
        path: &Path,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<Vec<MusicBrainzCandidate>, AudioIdentificationError> {
        assert!(!cancellation.is_cancelled());
        assert_eq!(std::fs::read(path).unwrap(), b"fLaCfixture");
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.paths.lock().unwrap().push(path.to_owned());
        Ok(vec![candidate()])
    }
}

fn candidate() -> MusicBrainzCandidate {
    MusicBrainzCandidate::new(
        "11111111-1111-4111-8111-111111111111",
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        0.95,
    )
    .unwrap()
}

/// Selects a mock-backed source without ticking or performing any remote request.
fn controller() -> (tempfile::TempDir, AppController) {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = Config::for_dir(temporary.path().join("config"));
    config.providers.acoustid_client_key = Some("fixture-key".to_owned());
    config.providers.funkwhale_instance_url = Some(url::Url::parse("https://pod.test/").unwrap());
    config.persistence.save_playback_history = false;
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    controller.view.screen = Screen::Funkwhale;
    select(&mut controller, 7);
    (temporary, controller)
}

fn select(controller: &mut AppController, id: u64) {
    let source = format!("https://pod.test/library/tracks/{id}");
    let summary = VideoSummary {
        video_id: source.clone(),
        title: "Fixture track".to_owned(),
        channel_name: "Artist".to_owned(),
        channel_id: "https://pod.test/library/artists/1".to_owned(),
        description: String::new(),
        duration_seconds: Some(146),
        view_count: None,
        published_at: None,
        published_text: None,
        live: false,
        was_live: false,
        orientation: VideoOrientation::Unknown,
        thumbnails: vec![],
        webpage_url: Some(url::Url::parse(&source).unwrap()),
        stream_url: None,
    };
    let mut item = controller.selected_video_queue_item(&summary, None);
    item.media.id.source = SourceKind::Funkwhale;
    item.media.kind = MediaKind::Audio;
    let mut snapshot = controller.funkwhale_navigation_snapshot(&item).unwrap();
    controller.swap_funkwhale_navigation(&mut snapshot);
    controller.view.selected = 0;
    controller.view.rows = vec![RowView {
        media_id: Some(MediaId::new(SourceKind::Funkwhale, source.clone())),
        title: "Fixture track".to_owned(),
        ..RowView::default()
    }];
    controller.view.details = Some(DetailView {
        media_id: Some(MediaId::new(SourceKind::Funkwhale, source.clone())),
        webpage_url: Some(url::Url::parse(&source).unwrap()),
        description: "Provider description and tags stay intact".to_owned(),
        links: vec![
            DetailLinkView {
                label: "Artist".to_owned(),
                url: "https://pod.test/library/artists/1".to_owned(),
                ..DetailLinkView::default()
            },
            DetailLinkView {
                label: "Album".to_owned(),
                url: "https://pod.test/library/albums/1".to_owned(),
                ..DetailLinkView::default()
            },
            DetailLinkView {
                label: "Uploaded by".to_owned(),
                url: "https://pod.test/federation/actors/uploader".to_owned(),
                ..DetailLinkView::default()
            },
            DetailLinkView {
                label: "Wikidata".to_owned(),
                url: "https://www.wikidata.org/wiki/Q1".to_owned(),
                wikidata_item_id: Some("Q1".to_owned()),
                ..DetailLinkView::default()
            },
        ],
        ..DetailView::default()
    });
}

/// Refuses a response that grows past its declared complete media size.
#[test]
fn staging_rejects_truncated_and_oversized_payloads() {
    let cancellation = AudioIdentificationCancellation::new();
    for (bytes, expected, limit) in [
        (b"fLaCshort".as_slice(), Some(100), 128),
        (b"fLaClonger".as_slice(), None, 5),
    ] {
        assert!(
            copy_audio(
                &mut Cursor::new(bytes),
                &mut Vec::new(),
                expected,
                limit,
                &cancellation,
            )
            .is_err()
        );
    }
}

/// A canceled request never copies audio or starts fingerprint work.
#[test]
fn staging_honors_cancellation_before_copying() {
    let cancellation = AudioIdentificationCancellation::new();
    cancellation.cancel();
    let mut destination = Vec::new();
    assert!(
        copy_audio(
            &mut Cursor::new(b"fLaCfixture"),
            &mut destination,
            None,
            128,
            &cancellation,
        )
        .is_err()
    );
    assert!(destination.is_empty());
}

/// EINTR is not a failed upload: resume the same reader after any already-copied bytes.
#[test]
fn interrupted_audio_reads_resume_the_same_body_without_duplicate_bytes() {
    struct InterruptedBody {
        input: Cursor<&'static [u8]>,
        calls: usize,
    }
    impl Read for InterruptedBody {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.calls == 2 {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let limit = output.len().min(3);
            self.input.read(&mut output[..limit])
        }
    }
    let bytes = b"fLaCfixture";
    let mut reader = InterruptedBody {
        input: Cursor::new(bytes.as_slice()),
        calls: 0,
    };
    let mut copied = Vec::new();
    let cancellation = AudioIdentificationCancellation::new();
    assert_eq!(
        copy_audio(
            &mut reader,
            &mut copied,
            Some(bytes.len() as u64),
            128,
            &cancellation
        )
        .unwrap(),
        bytes.len() as u64
    );
    assert_eq!(copied, bytes);
    assert_eq!(reader.input.position(), bytes.len() as u64);
}

/// Retry only EINTR, and revisit the original cancellation/deadline before doing another read.
#[test]
fn interrupted_audio_read_retry_observes_cancellation_and_does_not_retry_timeouts() {
    struct FailingBody<'a> {
        cancellation: &'a AudioIdentificationCancellation,
        kind: std::io::ErrorKind,
        calls: usize,
    }
    impl Read for FailingBody<'_> {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.kind == std::io::ErrorKind::Interrupted {
                self.cancellation.cancel();
            }
            Err(self.kind.into())
        }
    }
    let cancellation = AudioIdentificationCancellation::new();
    let mut reader = FailingBody {
        cancellation: &cancellation,
        kind: std::io::ErrorKind::Interrupted,
        calls: 0,
    };
    let error = copy_audio(&mut reader, &mut Vec::new(), None, 128, &cancellation).unwrap_err();
    assert!(error.contains("cancelled"));
    assert_eq!(reader.calls, 1);
    let cancellation = AudioIdentificationCancellation::new();
    let mut reader = FailingBody {
        cancellation: &cancellation,
        kind: std::io::ErrorKind::TimedOut,
        calls: 0,
    };
    assert!(copy_audio(&mut reader, &mut Vec::new(), None, 128, &cancellation).is_err());
    assert_eq!(reader.calls, 1);
}

/// Redirects and body reads cannot reset an expired whole-transfer budget.
#[test]
fn staging_uses_one_absolute_deadline_without_waiting() {
    let cancellation = AudioIdentificationCancellation::new();
    let mut reader = Cursor::new(b"fLaCfixture");
    let mut output = Vec::new();
    let deadline = Instant::now() - Duration::from_secs(1);
    assert!(transfer_time_left(deadline).is_err());
    assert!(
        copy_audio_until(
            &mut reader,
            &mut output,
            Some(11),
            128,
            &cancellation,
            deadline
        )
        .is_err()
    );
    assert!(output.is_empty());
    assert_eq!(reader.position(), 0);
}

/// Plain playlists cannot cause fpcalc's decoder to request nested URLs.
#[test]
fn remote_html_and_manifests_never_reach_the_identifier() {
    let cancellation = AudioIdentificationCancellation::new();
    for bytes in [
        b"#EXTM3U\nhttps://example.test/audio".as_slice(),
        b"  <html>login required</html>",
        b"[playlist]\nFile1=file:///etc/passwd",
    ] {
        assert!(
            copy_audio(
                &mut Cursor::new(bytes),
                &mut Vec::new(),
                None,
                128,
                &cancellation
            )
            .is_err()
        );
    }
    let bytes = b"fLaCfixture";
    let mut destination = Vec::new();
    assert_eq!(
        copy_audio(
            &mut Cursor::new(bytes),
            &mut destination,
            Some(11),
            128,
            &cancellation
        )
        .unwrap(),
        11
    );
    assert_eq!(destination, bytes);
}

/// Track pages cannot escape their selected pod or contain hidden credentials.
#[test]
fn fingerprint_source_requires_a_canonical_track_page() {
    for source in [
        "https://pod.test/library/tracks/0",
        "https://pod.test/library/tracks/01",
        "https://pod.test/library/artists/1",
        "https://user:password@pod.test/library/tracks/1",
        "https://pod.test/library/tracks/1?token=hidden",
        "https://pod.test/library/tracks/1#fragment",
    ] {
        assert!(
            source_pod(&url::Url::parse(source).unwrap()).is_err(),
            "{source}"
        );
    }
    assert_eq!(
        source_pod(&url::Url::parse("https://pod.test/prefix/library/tracks/1/").unwrap())
            .unwrap()
            .as_str(),
        "https://pod.test/prefix/",
    );
}

/// Descriptor parsing pins the upload UUID and never rewrites arbitrary CDN queries.
#[test]
fn fresh_descriptor_is_scoped_and_pins_an_exact_upload() {
    let source = url::Url::parse("https://pod.test/prefix/library/tracks/7").unwrap();
    let pod = source_pod(&source).unwrap();
    let raw = raw_audio(
        "/prefix/api/v2/listen/track-uuid/?download=true",
        UPLOAD,
        11,
    );
    let audio = audio_source(&source, &pod, &raw).unwrap();
    assert_eq!(audio.identity.duration, 146);
    assert_eq!(audio.identity.upload_uuid, UPLOAD);
    assert!(
        audio
            .url
            .query_pairs()
            .any(|(key, value)| key == "upload" && value == UPLOAD)
    );
    let mut mismatched = raw.clone();
    mismatched["id"] = serde_json::json!(8);
    assert!(audio_source(&source, &pod, &mismatched).is_err());
    for advertised in [
        "file:///etc/passwd",
        "https://user:secret@cdn.test/audio",
        "https://cdn.test/audio#fragment",
    ] {
        let raw = raw_audio(advertised, UPLOAD, 11);
        assert!(audio_source(&source, &pod, &raw).is_err());
    }
}

/// A smaller public rendition remains eligible when a lossless upload exceeds the cap.
#[test]
fn descriptor_prefers_a_rendition_within_the_download_bound() {
    let source = url::Url::parse("https://pod.test/library/tracks/7").unwrap();
    let pod = source_pod(&source).unwrap();
    let mut raw = raw_audio("/api/v2/listen/track-uuid/", UPLOAD, 11);
    let smaller = raw["uploads"][0].clone();
    raw["uploads"][0]["size"] = serde_json::json!(MAX_AUDIO_BYTES + 1);
    assert!(
        audio_source(&source, &pod, &raw)
            .err()
            .unwrap()
            .contains("128 MiB")
    );
    raw["uploads"][0]["uuid"] = serde_json::json!("27c770f0-1a57-40e9-85de-d2ca75b1d42d");
    raw["uploads"].as_array_mut().unwrap().push(smaller);
    let audio = audio_source(&source, &pod, &raw).unwrap();
    assert_eq!(audio.identity.size, Some(11));
    assert_eq!(audio.identity.upload_uuid, UPLOAD);
    assert!(
        audio
            .url
            .query_pairs()
            .any(|(key, value)| key == "upload" && value == UPLOAD)
    );
}

/// Identical verified revisions reuse results; changed upload revisions are identified again.
#[test]
fn explicit_worker_downloads_once_per_fresh_upload_revision_and_removes_staging() {
    let pod = Pod::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let paths = Arc::new(Mutex::new(Vec::new()));
    let mut identifier = Identifier {
        calls: calls.clone(),
        paths: paths.clone(),
    };
    let mut cache = WorkerCache::default();
    let cancellation = AudioIdentificationCancellation::new();
    identify_source(&pod.source(), &cancellation, &mut identifier, &mut cache).unwrap();
    identify_source(&pod.source(), &cancellation, &mut identifier, &mut cache).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        pod.requests.lock().unwrap().as_slice(),
        ["/api/v1/tracks/7/", "/audio", "/api/v1/tracks/7/"]
    );
    assert!(paths.lock().unwrap().iter().all(|path| !path.exists()));
    pod.revision.store(1, Ordering::Relaxed);
    identify_source(&pod.source(), &cancellation, &mut identifier, &mut cache).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert!(paths.lock().unwrap().iter().all(|path| !path.exists()));
}

/// A progressing full upload may take more than two seconds before fpcalc can inspect it.
#[test]
fn healthy_fingerprint_upload_can_take_more_than_two_seconds_and_keeps_staging_private() {
    struct PrivateIdentifier {
        paths: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl AudioIdentifier for PrivateIdentifier {
        fn identify(
            &mut self,
            path: &Path,
            cancellation: &AudioIdentificationCancellation,
        ) -> Result<Vec<MusicBrainzCandidate>, AudioIdentificationError> {
            assert!(!cancellation.is_cancelled());
            assert_eq!(std::fs::read(path).unwrap(), b"fLaCfixture");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
                assert_eq!(
                    path.parent()
                        .unwrap()
                        .metadata()
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
            }
            self.paths.lock().unwrap().push(path.to_owned());
            Ok(vec![candidate()])
        }
    }

    let pod = Pod::with_audio_delay(Duration::from_millis(400));
    let paths = Arc::new(Mutex::new(Vec::new()));
    let mut identifier = PrivateIdentifier {
        paths: Arc::clone(&paths),
    };
    let cancellation = AudioIdentificationCancellation::new();
    let mut cache = WorkerCache::default();
    let started = Instant::now();
    let result = identify_source(&pod.source(), &cancellation, &mut identifier, &mut cache);
    assert!(started.elapsed() > Duration::from_secs(2));
    let (_, candidates) =
        result.expect("Healthy progressing audio must use the whole transfer budget");
    assert_eq!(candidates, vec![candidate()]);
    assert_eq!(
        pod.requests.lock().unwrap().as_slice(),
        ["/api/v1/tracks/7/", "/audio"]
    );
    let paths = paths.lock().unwrap();
    assert_eq!(paths.len(), 1);
    assert!(
        paths
            .iter()
            .all(|path| !path.exists() && !path.parent().unwrap().exists())
    );
    cancellation.cancel();
    assert!(identify_source(&pod.source(), &cancellation, &mut identifier, &mut cache).is_err());
    assert_eq!(
        pod.requests.lock().unwrap().len(),
        2,
        "Cancellation is checked before any additional network request"
    );
}

/// Both providers use the same injected identifier rather than independent rate limiters.
#[test]
fn local_and_funkwhale_jobs_share_one_serial_identifier() {
    let pod = Pod::new();
    let directory = tempfile::tempdir().unwrap();
    let local = directory.path().join("local.flac");
    std::fs::write(&local, b"fLaCfixture").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let paths = Arc::new(Mutex::new(Vec::new()));
    let identifier = Identifier {
        calls: calls.clone(),
        paths,
    };
    let (requests, receiver) = bounded(3);
    let (responses, local_results) = unbounded();
    let state = State::new(WorkerNotifier::default());
    let worker =
        thread::spawn(move || local_fingerprint_worker(receiver, responses, Box::new(identifier)));
    requests
        .send(LocalFingerprintWorkerRequest::Identify {
            generation: 1,
            path: local.clone(),
            cancellation: AudioIdentificationCancellation::new(),
        })
        .unwrap();
    requests
        .send(LocalFingerprintWorkerRequest::IdentifyFunkwhale(Request {
            generation: 2,
            source: pod.source(),
            cancellation: AudioIdentificationCancellation::new(),
            responses: state.sender.clone(),
        }))
        .unwrap();
    requests
        .send(LocalFingerprintWorkerRequest::Shutdown)
        .unwrap();
    let local_result = local_results.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(local_result.path, local);
    assert!(local_result.result.is_ok());
    assert!(
        state
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .result
            .is_ok()
    );
    worker.join().unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert!(
        local.exists(),
        "remote staging cleanup must not delete Local input"
    );
}

/// On-demand presentation does not enqueue work and retains all provider metadata links.
#[test]
fn capability_and_cached_projection_preserve_artist_album_and_uploader_links() {
    let (_temporary, mut controller) = controller();
    let (sender, requests) = bounded(1);
    controller.local_fingerprint_requests = Some(sender);
    controller.local_fingerprint_request_drain = requests.clone();
    controller.apply_funkwhale_fingerprint_details();
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .local_fingerprint_available
    );
    assert!(
        requests.is_empty(),
        "selection must never stage or fingerprint audio"
    );
    let source = controller.selected_funkwhale_fingerprint_source().unwrap();
    controller.funkwhale_fingerprint.remember(
        UploadIdentity {
            source: source.to_string(),
            upload_uuid: UPLOAD.to_owned(),
            size: Some(11),
            duration: 146,
        },
        vec![candidate()],
    );
    controller.apply_funkwhale_fingerprint_details();
    controller.apply_funkwhale_fingerprint_details();
    let details = controller.view.details.as_ref().unwrap();
    assert_eq!(details.links.len(), 5, "projection must be idempotent");
    assert_eq!(details.links[0].label, "Artist");
    assert_eq!(details.links[1].label, "Album");
    assert_eq!(details.links[2].label, "Uploaded by");
    assert_eq!(details.links[3].wikidata_item_id.as_deref(), Some("Q1"));
    assert_eq!(details.links[4].url, candidate().url().as_str());
    assert_eq!(
        details.description,
        "Provider description and tags stay intact"
    );
    controller.local_fingerprint_requests = None;
}

/// A delayed Local cache revalidation cannot erase the newly selected online source.
#[test]
fn delayed_local_fingerprint_cleanup_preserves_online_details() {
    let (temporary, mut controller) = controller();
    let path = temporary.path().join("previous.flac");
    std::fs::write(&path, b"fLaCfixture").unwrap();
    controller.displayed_local_fingerprint = Some(LocalFingerprintCacheKey {
        identity: local_file_identity(&path).unwrap(),
        path,
    });
    controller.local_fingerprint_revalidate_at = Some(Instant::now());
    controller.view.selected_detail_link = Some(1);
    controller
        .view
        .details
        .as_mut()
        .unwrap()
        .loading_wikidata_item = Some("Q1".to_owned());
    #[cfg(feature = "wikidata")]
    let wikidata_generation = controller.wikidata_generation;
    controller.revalidate_displayed_local_fingerprint(Instant::now());
    let details = controller.view.details.as_ref().unwrap();
    assert_eq!(details.links.len(), 4);
    assert_eq!(details.loading_wikidata_item.as_deref(), Some("Q1"));
    assert_eq!(controller.view.selected_detail_link, Some(1));
    #[cfg(feature = "wikidata")]
    assert_eq!(controller.wikidata_generation, wikidata_generation);
}

/// Selecting a Local folder still discards an old file's recording link and cursor.
#[test]
fn local_folder_cleanup_resets_obsolete_fingerprint_link_selection() {
    let (_temporary, mut controller) = controller();
    controller.view.screen = Screen::Local;
    controller.view.selected_detail_link = Some(0);
    controller.view.details.as_mut().unwrap().media_id = None;
    controller.apply_local_fingerprint_details();
    assert!(controller.view.details.as_ref().unwrap().links.is_empty());
    assert_eq!(controller.view.selected_detail_link, None);
}

/// Missing lookup credentials show existing setup guidance before any public audio request.
#[test]
fn missing_acoustid_key_does_not_enqueue_a_download() {
    let (_temporary, mut controller) = controller();
    let (sender, requests) = bounded(1);
    controller.local_fingerprint_requests = Some(sender);
    controller.local_fingerprint_request_drain = requests.clone();
    controller.config.providers.acoustid_client_key = None;
    controller.fingerprint_selected_local_audio();
    assert!(requests.is_empty());
    assert!(controller.funkwhale_fingerprint.pending.is_none());
    assert!(controller.view.error_popup.is_some());
    controller.local_fingerprint_requests = None;
}

/// Selection and pod changes revoke queued work before stale results can alter Details.
#[test]
fn selection_changes_cancel_pending_work_and_discard_obsolete_results() {
    let (_temporary, mut controller) = controller();
    controller.shutdown_local_fingerprint_worker();
    let (sender, requests) = bounded(1);
    controller.local_fingerprint_requests = Some(sender);
    controller.local_fingerprint_request_drain = requests.clone();
    controller.fingerprint_selected_local_audio();
    let LocalFingerprintWorkerRequest::IdentifyFunkwhale(request) = requests.try_recv().unwrap()
    else {
        panic!("expected remote fingerprint request");
    };
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .local_fingerprint_pending
    );
    select(&mut controller, 8);
    controller.apply_funkwhale_fingerprint_details();
    assert!(request.cancellation.is_cancelled());
    request
        .responses
        .send(Completion {
            generation: request.generation,
            source: request.source.clone(),
            result: Ok((
                UploadIdentity {
                    source: request.source.to_string(),
                    upload_uuid: UPLOAD.to_owned(),
                    size: Some(11),
                    duration: 146,
                },
                vec![candidate()],
            )),
        })
        .unwrap();
    controller.drain_funkwhale_fingerprint_responses();
    assert_eq!(controller.view.details.as_ref().unwrap().links.len(), 4);
    assert!(controller.funkwhale_fingerprint.results.is_empty());
    controller.config.providers.funkwhale_instance_url =
        Some(url::Url::parse("https://other.test/").unwrap());
    controller.apply_funkwhale_fingerprint_details();
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .local_fingerprint_available
    );
    controller.local_fingerprint_requests = None;
}

/// A revealed playing item keeps its original pod even after the preference changes.
#[test]
fn original_selected_track_owns_identification_after_pod_preference_changes() {
    let (_temporary, mut controller) = controller();
    controller.shutdown_local_fingerprint_worker();
    let (sender, requests) = bounded(1);
    controller.local_fingerprint_requests = Some(sender);
    controller.local_fingerprint_request_drain = requests.clone();
    controller.config.providers.funkwhale_instance_url =
        Some(url::Url::parse("https://other.test/").unwrap());
    controller.apply_funkwhale_fingerprint_details();
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .local_fingerprint_available
    );
    controller.fingerprint_selected_local_audio();
    let LocalFingerprintWorkerRequest::IdentifyFunkwhale(request) = requests.try_recv().unwrap()
    else {
        panic!("expected original-pod fingerprint request");
    };
    assert_eq!(request.source.as_str(), "https://pod.test/library/tracks/7");
    controller.view.rows[0].media_id = Some(MediaId::new(
        SourceKind::Funkwhale,
        "https://other.test/library/tracks/7",
    ));
    controller.apply_funkwhale_fingerprint_details();
    assert!(
        !controller
            .view
            .details
            .as_ref()
            .unwrap()
            .local_fingerprint_available
    );
    assert!(request.cancellation.is_cancelled());
    controller.local_fingerprint_requests = None;
}

/// Cache keys include the source pod and every advertised upload revision fact.
#[test]
fn revision_cache_is_bounded_and_never_crosses_pods() {
    let mut cache = WorkerCache::default();
    for id in 0..MAX_CACHED_UPLOADS + 1 {
        cache.remember(
            UploadIdentity {
                source: format!("https://pod.test/library/tracks/{id}"),
                upload_uuid: UPLOAD.to_owned(),
                size: Some(11),
                duration: 146,
            },
            vec![candidate()],
        );
    }
    assert_eq!(cache.results.len(), MAX_CACHED_UPLOADS);
    assert!(!cache.results.keys().any(|key| key.source.ends_with("/0")));
    let identity = UploadIdentity {
        source: "https://other.test/library/tracks/1".to_owned(),
        upload_uuid: UPLOAD.to_owned(),
        size: Some(11),
        duration: 146,
    };
    assert!(!cache.results.contains_key(&identity));
}
