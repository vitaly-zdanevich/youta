//! Anonymous REST and configured-helper fixtures; no external media or downloader.

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// A bounded local pod that records which original-pod API routes were requested.
struct Pod {
    base: Url,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Pod {
    /// Returns fresh audio URLs or a supplied unavailable/unsafe advertised locator.
    fn new(advertised: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = Url::parse(&format!(
            "http://{}/original/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let origin = base.clone();
        let advertised = advertised.map(str::to_owned);
        let worker = std::thread::spawn(move || {
            let started = Instant::now();
            while !stopping.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(10) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("mock pod accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                drop(reader);
                let count = {
                    let mut requests = recorded.lock().unwrap();
                    requests.push(path.clone());
                    requests.len()
                };
                let stream = match advertised.as_deref() {
                    Some("missing") => None,
                    Some(value) => Some(value.to_owned()),
                    None => Some(
                        origin
                            .join(&format!("fresh-{count}.flac?token=fixture"))
                            .unwrap()
                            .to_string(),
                    ),
                };
                let body = serde_json::json!({
                    "id": 42, "title": "Fixture recording", "artist_credit": [{"name": "Artist"}],
                    "is_playable": true, "listen_url": stream, "uploads": []
                })
                .to_string();
                let status = if path == "/original/api/v1/tracks/42/" {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            base,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    /// Matches the provider's slashless original page, retaining its pod prefix.
    fn page(&self) -> Url {
        self.base.join("library/tracks/42").unwrap()
    }
}

impl Drop for Pod {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

/// Publishes a closed executable fixture before probing readiness, avoiding ETXTBSY races.
fn config(directory: &Path, hold: bool) -> Config {
    let mut fixture = tempfile::Builder::new()
        .prefix("configured-ffmpeg-")
        .tempfile_in(directory)
        .unwrap();
    write!(fixture, "#!/bin/sh\nif [ \"${{1-}}\" = '__ready__' ]; then exit 0; fi\nset -eu\nprintf '%s\\n' \"$@\" >> \"$0.arguments\"\nfor argument in \"$@\"; do output=$argument; done\nprintf '%s' \"$output\" > \"$0.started\"\nprintf 'fixture opus' > \"$output\"\nprintf 'total_size=12\\n'\n{}\n", if hold { "while :; do sleep 1; done" } else { "sleep 0.05" }).unwrap();
    fixture.as_file().sync_all().unwrap();
    fixture
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .unwrap();
    let helper = fixture.into_temp_path().keep().unwrap();
    let started = Instant::now();
    loop {
        match Command::new(&helper).arg("__ready__").status() {
            Ok(status) => {
                assert!(status.success());
                break;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && started.elapsed() < Duration::from_secs(1) =>
            {
                std::thread::yield_now()
            }
            Err(error) => panic!("fixture FFmpeg not executable: {error}"),
        }
    }
    let mut config = Config::for_dir(directory.join("config"));
    config.providers.ffmpeg_executable = helper;
    config.providers.yt_dlp_executable = directory.join("downloader-must-not-run");
    config.providers.funkwhale_instance_url =
        Some(Url::parse("https://different-pod.invalid/").unwrap());
    config
}

/// Uses the same media fixture as the pure identity tests.
fn media(pod: &Pod) -> MediaItem {
    super::tests::selected(pod.page().as_str())
}

/// Approval-time staging re-resolves the original pod rather than downloading its HTML.
#[test]
fn fresh_native_audio_is_staged_privately_and_cleanup_follows_the_guard() {
    let directory = crate::test_support::canonical_tempdir("Funkwhale archive staging");
    let config = config(directory.path(), false);
    let pod = Pod::new(None);
    let media = media(&pod);
    for count in 1..=2 {
        let mut updates = Vec::new();
        let prepared = prepare_funkwhale_archive_media(
            &config,
            &media,
            media.webpage_url.as_str(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |bytes, total| updates.push((bytes, total)),
        )
        .unwrap();
        assert_eq!(prepared.filename(), "media.opus");
        assert_eq!(std::fs::read(prepared.path()).unwrap(), b"fixture opus");
        assert!(updates.contains(&(12, None)), "{updates:?}");
        assert_eq!(updates.last(), Some(&(12, Some(12))));
        assert_eq!(
            prepared
                .path()
                .parent()
                .unwrap()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let path = prepared.path().to_owned();
        drop(prepared);
        assert!(!path.exists());
        let args = std::fs::read_to_string(format!(
            "{}.arguments",
            config.providers.ffmpeg_executable.display()
        ))
        .unwrap();
        assert!(args.contains(&format!("fresh-{count}.flac?token=fixture")));
        assert!(!args.contains("library/tracks"));
    }
    assert_eq!(
        *pod.requests.lock().unwrap(),
        vec!["/original/api/v1/tracks/42/"; 2]
    );
}

/// Missing or unsafe advertised audio cannot fall back to HTML or a downloader.
#[test]
fn unavailable_or_unsafe_audio_never_starts_ffmpeg() {
    for advertised in [
        "missing",
        "file:///etc/passwd",
        "concat:https://audio.invalid/a|file:///etc/passwd",
    ] {
        let directory = crate::test_support::canonical_tempdir("Funkwhale rejected upload audio");
        let config = config(directory.path(), false);
        let pod = Pod::new(Some(advertised));
        let media = media(&pod);
        let error = prepare_funkwhale_archive_media(
            &config,
            &media,
            media.webpage_url.as_str(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap_err();
        assert!(error.contains("Funkwhale"), "{error}");
        assert!(
            !PathBuf::from(format!(
                "{}.started",
                config.providers.ffmpeg_executable.display()
            ))
            .exists()
        );
    }
}

/// Canceling an active helper terminates its process group and removes its output.
#[test]
fn canceled_ffmpeg_is_terminated_and_private_output_is_removed() {
    let directory = crate::test_support::canonical_tempdir("Funkwhale staging cancellation");
    let config = config(directory.path(), true);
    let pod = Pod::new(None);
    let media = media(&pod);
    let cancellation = Arc::new(AtomicBool::new(false));
    let signal = cancellation.clone();
    let started_file = PathBuf::from(format!(
        "{}.started",
        config.providers.ffmpeg_executable.display()
    ));
    let signal_file = started_file.clone();
    let cancel = std::thread::spawn(move || {
        let started = Instant::now();
        while !signal_file.exists() && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(2));
        }
        signal.store(true, Ordering::Relaxed);
    });
    let error = prepare_funkwhale_archive_media(
        &config,
        &media,
        media.webpage_url.as_str(),
        false,
        &cancellation,
        |_, _| {},
    )
    .unwrap_err();
    cancel.join().unwrap();
    assert!(error.contains("canceled"), "{error}");
    let path = std::fs::read_to_string(started_file).unwrap();
    assert!(!Path::new(&path).exists());
}

/// Native staging inherits the same output-size and entire-operation deadline limits.
#[test]
fn output_limits_and_deadlines_remove_incomplete_audio() {
    for (max_bytes, deadline, message) in [
        (4, Duration::from_secs(3), "size limit"),
        (1024, Duration::from_millis(300), "timed out"),
    ] {
        let directory = crate::test_support::canonical_tempdir("Funkwhale staging limits");
        let config = config(directory.path(), true);
        let pod = Pod::new(None);
        let media = media(&pod);
        let error = prepare_with_limits(
            &config,
            &media,
            media.webpage_url.as_str(),
            false,
            &Arc::new(AtomicBool::new(false)),
            |_, _| {},
            max_bytes,
            deadline,
        )
        .unwrap_err();
        assert!(error.contains(message), "{error}");
        if let Ok(path) = std::fs::read_to_string(format!(
            "{}.started",
            config.providers.ffmpeg_executable.display()
        )) {
            assert!(!Path::new(&path).exists());
        }
    }
}

/// Commons uses the same native supervised route, never the generic page downloader.
#[cfg(feature = "commons-upload")]
#[test]
fn commons_opus_staging_uses_the_advertised_audio_without_yt_dlp() {
    let directory = crate::test_support::canonical_tempdir("Funkwhale Commons staging");
    let config = config(directory.path(), false);
    let pod = Pod::new(None);
    let staging = directory.path().join("reviewed-staging");
    let path = crate::opus_export::prepare_provider_opus(
        &crate::opus_export::OpusAudioSource::Funkwhale(pod.page()),
        &staging,
        &config.providers.yt_dlp_executable,
        &config.providers.ffmpeg_executable,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(path.file_name().unwrap(), "audio.opus");
    assert_eq!(std::fs::read(path).unwrap(), b"fixture opus");
    assert_eq!(
        *pod.requests.lock().unwrap(),
        vec!["/original/api/v1/tracks/42/"]
    );
}
