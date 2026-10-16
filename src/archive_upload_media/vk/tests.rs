//! Offline VK export tests with synthetic metadata and a configured fake helper.

use super::*;
use crate::domain::{MediaId, MediaLicense, MediaStatistics};
use crate::providers::vk::VkPage;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// Records native requests and can hold authentication until cancellation is visible.
struct MockResolver {
    calls: Arc<Mutex<Vec<&'static str>>>,
    started: Option<std::sync::mpsc::Sender<()>>,
    release: Option<std::sync::mpsc::Receiver<()>>,
}

impl AudioResolver for MockResolver {
    fn authenticate(&mut self) -> Result<(), String> {
        self.calls.lock().unwrap().push("authenticate");
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
        }
        if let Some(release) = self.release.take() {
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Ok(())
    }

    fn search(&mut self, _query: &str) -> Result<VkPage, String> {
        self.calls.lock().unwrap().push("search");
        Ok(VkPage {
            tracks: Vec::new(),
            next: None,
        })
    }

    fn resolve(&mut self, _track: &VkTrack) -> Result<VkPlaybackUrl, String> {
        self.calls.lock().unwrap().push("resolve");
        Ok(audio())
    }
}

/// Cancelled detached workers must stop before the next authenticated request.
#[test]
fn cancellation_during_authentication_prevents_followup_vk_requests() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
    let mut resolver = MockResolver {
        calls: calls.clone(),
        started: Some(started_sender),
        release: Some(release_receiver),
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let external = cancellation.clone();
    let waiting = std::thread::spawn(move || {
        wait_for_resolution(
            &external,
            Instant::now(),
            Duration::from_secs(5),
            move |cancelled, started, deadline| {
                let result = refresh_audio(
                    &mut resolver,
                    VkAudioId::new(-123, 456).unwrap(),
                    "Track Artist",
                    &cancelled,
                    started,
                    deadline,
                );
                finished_sender.send(result.is_err()).unwrap();
                result
            },
        )
    });
    started_receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    cancellation.store(true, Ordering::Release);
    assert!(waiting.join().unwrap().is_err());
    release_sender.send(()).unwrap();
    assert!(
        finished_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    );
    assert_eq!(*calls.lock().unwrap(), ["authenticate"]);
}

/// A whole-operation timeout also revokes work that completes after the waiter exits.
#[test]
fn expired_authentication_cannot_start_search_or_resolution() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
    let mut resolver = MockResolver {
        calls: calls.clone(),
        started: Some(started_sender),
        release: Some(release_receiver),
    };
    let waiting = std::thread::spawn(move || {
        wait_for_resolution(
            &AtomicBool::new(false),
            Instant::now(),
            Duration::from_millis(100),
            move |cancelled, started, deadline| {
                let result = refresh_audio(
                    &mut resolver,
                    VkAudioId::new(-123, 456).unwrap(),
                    "Track Artist",
                    &cancelled,
                    started,
                    deadline,
                );
                finished_sender.send(result.is_err()).unwrap();
                result
            },
        )
    });
    started_receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(waiting.join().unwrap().is_err());
    release_sender.send(()).unwrap();
    assert!(
        finished_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    );
    assert_eq!(*calls.lock().unwrap(), ["authenticate"]);
}

/// Existing selected rows resolve directly even when a fresh global search would be empty.
#[test]
fn retained_session_resolution_never_reauthenticates_or_searches() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut resolver = MockResolver {
        calls: calls.clone(),
        started: None,
        release: None,
    };
    let track = VkTrack::fixture(VkAudioId::new(-123, 456).unwrap(), "Track", "Artist");
    let result = resolve_selected(
        &mut resolver,
        &track,
        &AtomicBool::new(false),
        Instant::now(),
        Duration::from_secs(5),
    );
    assert!(result.is_ok());
    assert_eq!(*calls.lock().unwrap(), ["resolve"]);
}

/// Selected private reload material never enters export-session diagnostics.
#[test]
fn retained_export_source_debug_contains_only_canonical_identity() {
    let cookies = crate::providers::vk::VkCookieJar::parse(
        b".vk.ru\tTRUE\t/\tTRUE\t0\tsid\tprivate-fixture-cookie\n",
    )
    .unwrap();
    let client = VkClient::from_cookies(cookies);
    let source = VkExportSource::new(
        client,
        VkTrack::fixture(VkAudioId::new(-123, 456).unwrap(), "Track", "Artist"),
    );
    assert_eq!(source.webpage_url().as_str(), "https://vk.ru/audio-123_456");
    let debug = format!("{source:?}");
    assert!(debug.contains("-123_456"));
    for secret in [
        "private-fixture-cookie",
        "fixture-action",
        "fixture-locator",
        "Track",
        "Artist",
    ] {
        assert!(!debug.contains(secret), "{secret}");
    }
}

/// Changed review provenance cannot resolve the captured session's different recording.
#[test]
fn retained_export_rejects_a_mismatching_review_before_resolution() {
    let cookies =
        crate::providers::vk::VkCookieJar::parse(b".vk.ru\tTRUE\t/\tTRUE\t0\tsid\tfixture\n")
            .unwrap();
    let source = VkExportSource::new(
        VkClient::from_cookies(cookies),
        VkTrack::fixture(VkAudioId::new(-123, 999).unwrap(), "Other track", "Artist"),
    );
    let directory = crate::test_support::canonical_tempdir("VK mismatched retained export");
    let selected = media();
    let error = prepare_vk_archive_media_from_session(
        &Config::for_dir(directory.path()),
        &selected,
        selected.webpage_url.as_str(),
        false,
        &Arc::new(AtomicBool::new(false)),
        |_, _| panic!("mismatched source cannot prepare"),
        &source,
    )
    .unwrap_err();
    assert!(error.contains("no longer matches"));
}

/// Creates a canonical selected audio row without private provider state.
fn media() -> MediaItem {
    MediaItem {
        id: MediaId::new(SourceKind::Vk, "-123_456"),
        kind: MediaKind::Audio,
        title: "Fixture track".to_owned(),
        creator: Some("Fixture artist".to_owned()),
        description: None,
        webpage_url: Url::parse("https://vk.ru/audio-123_456").unwrap(),
        thumbnail_url: None,
        duration_seconds: Some(120),
        published_at: None,
        statistics: MediaStatistics::default(),
        license: MediaLicense::Unknown,
        chapters: Vec::new(),
        captions: Vec::new(),
    }
}

/// Returns a signed fixture through the same URL validation as real resolution.
fn audio() -> VkPlaybackUrl {
    crate::providers::vk::decode_audio_url(
        "https://psv4.vkuseraudio.ru/fixture/index.m3u8?token=fixture-secret",
        123,
    )
    .unwrap()
}

/// Provenance accepts only the application's exact, credential-free public spelling.
#[test]
fn canonical_vk_source_rejects_private_or_ambiguous_urls() {
    assert_eq!(
        canonical_source(&media().webpage_url).unwrap().to_string(),
        "-123_456"
    );
    for source in [
        "https://vk.ru/audio-123_456?access_hash=private",
        "https://vk.ru/audio-123_456#private",
        "https://vk.com/audio-123_456",
        "https://user:secret@vk.ru/audio-123_456",
        "https://vk.ru/audio-123_456/",
        "https://vk.ru/audio-0123_456",
        "https://vk.ru.evil.test/audio-123_456",
        "https://psv4.vkuseraudio.ru/private.mp3?token=secret",
    ] {
        assert!(
            canonical_source(&Url::parse(source).unwrap()).is_err(),
            "{source}"
        );
    }
}

/// A bounded refresh must prove the selected owner/audio pair before obtaining media.
#[test]
fn refreshed_search_requires_the_exact_selected_identity() {
    let selected = VkAudioId::new(-123, 456).unwrap();
    let other = VkAudioId::new(-123, 457).unwrap();
    let row = |id| VkTrack::fixture(id, "Same title", "Same artist");
    assert!(
        exact_track(
            VkPage {
                tracks: vec![row(other)],
                next: None
            },
            selected
        )
        .is_err()
    );
    let found = exact_track(
        VkPage {
            tracks: vec![row(other), row(selected)],
            next: None,
        },
        selected,
    )
    .unwrap();
    assert_eq!(found.id, selected);
}

/// Invalid identity, changed locators and video requests cannot start resolution.
#[test]
fn invalid_vk_export_never_resolves_or_starts_helpers() {
    let directory = crate::test_support::canonical_tempdir("VK export rejected");
    let config = Config::for_dir(directory.path());
    let mut changed = media();
    changed.id.external_id = "-123_999".to_owned();
    for (selected, location, video) in [
        (changed, media().webpage_url.to_string(), false),
        (media(), "https://vk.ru/audio-123_999".to_owned(), false),
        (media(), media().webpage_url.to_string(), true),
    ] {
        assert!(
            prepare_with_resolver(
                &config,
                &selected,
                &location,
                video,
                &Arc::new(AtomicBool::new(false)),
                |_, _| {},
                || panic!("invalid selections cannot resolve"),
            )
            .is_err()
        );
    }
}

/// Cancellation prevents any authenticated request, staging or helper launch.
#[test]
fn canceled_vk_export_never_resolves() {
    let directory = crate::test_support::canonical_tempdir("VK export canceled");
    let selected = media();
    assert!(
        prepare_with_resolver(
            &Config::for_dir(directory.path()),
            &selected,
            selected.webpage_url.as_str(),
            false,
            &Arc::new(AtomicBool::new(true)),
            |_, _| {},
            || panic!("canceled exports cannot resolve"),
        )
        .is_err()
    );
}

/// Helpers receive a vetted CDN URL, never cookies, a cookie-file path or VK HTML.
#[test]
fn vk_ffmpeg_command_uses_only_the_resolved_audio() {
    let command = ffmpeg_command(Path::new("ffmpeg"), &audio(), Path::new("audio.opus"));
    let arguments: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert!(
        arguments
            .iter()
            .any(|arg| arg.contains("index.m3u8?token=fixture-secret"))
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["-protocol_whitelist", "https,tcp,tls,crypto"])
    );
    for forbidden in ["Cookie", "cookies", "-headers", "vk.ru/audio", "yt-dlp"] {
        assert!(
            !arguments.iter().any(|arg| arg.contains(forbidden)),
            "{forbidden}"
        );
    }
}

/// Makes a closed, executable fixture that records arguments without network access.
#[cfg(unix)]
fn config_with_helper(directory: &Path, fail: bool) -> Config {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = tempfile::Builder::new()
        .prefix("vk-ffmpeg-")
        .tempfile_in(directory)
        .unwrap();
    writeln!(fixture, "#!/bin/sh\nif [ \"${{1-}}\" = '__ready__' ]; then exit 0; fi\nset -eu\nprintf '%s\\n' \"$@\" > \"$0.arguments\"\nfor argument in \"$@\"; do output=$argument; done\nprintf '%s\\n' 'https://psv4.vkuseraudio.ru/private?token=fixture-secret' >&2\n{}", if fail { "exit 7" } else { "printf 'fixture opus' > \"$output\"\nprintf 'total_size=12\\n'" }).unwrap();
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
            Err(error) => panic!("fixture helper: {error}"),
        }
    }
    let mut config = Config::for_dir(directory.join("config"));
    config.providers.ffmpeg_executable = helper;
    config.providers.yt_dlp_executable = directory.join("yt-dlp-must-not-run");
    config
}

/// A private output stays alive through publication and is removed with its guard.
#[cfg(unix)]
#[test]
fn native_vk_audio_uses_private_staging_and_guard_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    let directory = crate::test_support::canonical_tempdir("VK export staging");
    let config = config_with_helper(directory.path(), false);
    let selected = media();
    let prepared = prepare_with_resolver(
        &config,
        &selected,
        selected.webpage_url.as_str(),
        false,
        &Arc::new(AtomicBool::new(false)),
        |_, _| {},
        || Ok(audio()),
    )
    .unwrap();
    assert_eq!(prepared.filename(), "media.opus");
    assert_eq!(std::fs::read(prepared.path()).unwrap(), b"fixture opus");
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
}

/// The generic supervisor must not echo signed input URLs from helper diagnostics.
#[cfg(unix)]
#[test]
fn failed_vk_helper_does_not_disclose_signed_audio() {
    let directory = crate::test_support::canonical_tempdir("VK export diagnostic");
    let config = config_with_helper(directory.path(), true);
    let selected = media();
    let error = prepare_with_resolver(
        &config,
        &selected,
        selected.webpage_url.as_str(),
        false,
        &Arc::new(AtomicBool::new(false)),
        |_, _| {},
        || Ok(audio()),
    )
    .unwrap_err();
    assert!(error.contains("failed"));
    assert!(!error.contains("fixture-secret"));
    assert!(!error.contains("vkuseraudio"));
}
