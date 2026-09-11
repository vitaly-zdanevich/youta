//! Regression tests for selected-only, asynchronous Web metadata projection.

use super::*;
use crate::web_browser::{WebDirectoryListing, WebEntry, WebEntryKind};
use crate::web_metadata::WebMediaMetadata;

/// Opens a deterministic listing without contacting a real media server.
fn metadata_controller() -> AppController {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.show_screen(Screen::Web);
    let url = url::Url::parse("http://127.0.0.1:9/music/").unwrap();
    controller.web.generation = 1;
    controller.web.pending = true;
    controller.handle_web_response(
        1,
        Ok(WebDirectoryListing {
            url: url.clone(),
            parent: Some(url.join("../").unwrap()),
            entries: vec![
                WebEntry {
                    url: url.join("album/").unwrap(),
                    name: "album".into(),
                    kind: WebEntryKind::Directory,
                },
                WebEntry {
                    url: url.join("01.opus?token=fixture").unwrap(),
                    name: "01.opus".into(),
                    kind: WebEntryKind::Audio,
                },
                WebEntry {
                    url: url.join("02.mp4").unwrap(),
                    name: "02.mp4".into(),
                    kind: WebEntryKind::Video,
                },
            ],
            truncated: false,
        }),
    );
    controller
}

/// Supplies the same fields and units shown by the Local details panel.
fn tagged_metadata() -> WebMediaMetadata {
    WebMediaMetadata {
        title: Some("Track title".into()),
        artist: Some("Artist".into()),
        album: Some("Album".into()),
        genre: Some("Speech".into()),
        comment: Some("A recording".into()),
        duration: Some(Duration::from_secs(125)),
        container: Some("Ogg".into()),
        codec: Some("Opus".into()),
        size_bytes: Some(1_048_576),
        bitrate_kbps: Some(96),
        sample_rate_hz: Some(48_000),
        channels: Some(2),
        ..WebMediaMetadata::default()
    }
}

#[test]
fn web_metadata_uses_local_labels_without_local_actions_or_reordering_files() {
    let mut controller = metadata_controller();
    controller.select_row(2);
    let entry = controller.selected_web_entry().unwrap().clone();
    controller.cache_web_metadata(entry.url.clone(), tagged_metadata(), None);
    controller.update_web_detail();
    let details = controller.view.details.as_ref().unwrap();
    assert_eq!(details.title, "Track title");
    assert_eq!(details.source, "Web");
    assert_eq!(details.length, "2:05");
    for field in [
        "Artists: Artist",
        "Album: Album",
        "Genre: Speech",
        "Comment: A recording",
        "Length: 2:05",
        "Container: Ogg",
        "Codec: Opus",
        "Size:",
        "Bitrate: 96 kb/s",
        "Sample rate: 48000 Hz",
        "Channels: 2",
    ] {
        assert!(
            details.description.contains(field),
            "missing {field}: {}",
            details.description
        );
    }
    assert!(!details.description.contains("Full path:"));
    assert_eq!(details.webpage_url.as_ref(), Some(&entry.url));
    assert!(!details.local_renamable && !details.local_movable && !details.local_trashable);
    assert!(!details.local_audio_quality_available && !details.local_fingerprint_available);
    assert!(details.local_video_thumbnail.is_none());
    assert_eq!(controller.view.rows[2].title, "01.opus");
    assert_eq!(controller.view.rows[3].title, "02.mp4");
}

#[test]
fn web_metadata_missing_fields_keep_filename_and_do_not_invent_zero_size() {
    let mut controller = metadata_controller();
    controller.select_row(2);
    let url = controller.selected_web_entry().unwrap().url.clone();
    controller.cache_web_metadata(url, WebMediaMetadata::default(), None);
    controller.update_web_detail();
    let details = controller.view.details.as_ref().unwrap();
    assert_eq!(details.title, "01.opus");
    assert!(!details.description.contains("Size: 0"));
    assert!(!details.description.contains("Artists:"));
    assert!(!details.description.contains("Loading metadata"));
    assert!(controller.selected_queue_item().is_ok());
}

#[test]
fn web_metadata_is_debounced_and_only_latest_playable_selection_is_pending() {
    let mut controller = metadata_controller();
    assert!(controller.web.metadata.request.is_none());
    controller.select_row(1);
    assert!(controller.web.metadata.request.is_none());
    controller.select_row(2);
    controller.poll_web_worker();
    assert!(
        controller.web.metadata.worker.is_none(),
        "debounce must prevent immediate network access"
    );
    controller.select_row(3);
    assert!(
        controller
            .web
            .metadata
            .request
            .as_ref()
            .unwrap()
            .0
            .path()
            .ends_with("02.mp4")
    );
    controller.select_row(0);
    assert!(controller.web.metadata.request.is_none());
    assert!(controller.web.metadata.worker.is_none());
}

#[test]
fn web_metadata_cached_selection_is_immediate_and_does_not_refetch() {
    let mut controller = metadata_controller();
    controller.select_row(2);
    let url = controller.selected_web_entry().unwrap().url.clone();
    controller.cache_web_metadata(url, tagged_metadata(), None);
    controller.select_row(3);
    controller.select_row(2);
    assert_eq!(
        controller.view.details.as_ref().unwrap().title,
        "Track title"
    );
    assert!(controller.web.metadata.request.is_none());
}

#[test]
fn web_metadata_old_results_cannot_replace_another_selected_item_or_tab() {
    let mut controller = metadata_controller();
    controller.select_row(2);
    let url = controller.selected_web_entry().unwrap().url.clone();
    controller.select_row(3);
    controller.cache_web_metadata(url, tagged_metadata(), None);
    controller.update_web_detail();
    assert_eq!(controller.view.details.as_ref().unwrap().title, "02.mp4");
    controller.show_screen(Screen::Playlists);
    let details = controller.view.details.clone();
    controller.poll_web_worker();
    assert_eq!(controller.view.details, details);
    assert!(controller.web.metadata.request.is_none());
}

#[test]
fn web_metadata_cache_has_a_fixed_entry_limit() {
    let mut controller = metadata_controller();
    for index in 0..140 {
        controller.cache_web_metadata(
            url::Url::parse(&format!("http://127.0.0.1/{index}.opus")).unwrap(),
            tagged_metadata(),
            None,
        );
    }
    assert_eq!(controller.web.metadata.cache.len(), 128);
}

/// Drives the real worker with an immediate deadline instead of sleeping through debounce.
fn start_metadata_now(controller: &mut AppController) {
    controller.web.metadata.request.as_mut().unwrap().1 = Instant::now();
    controller.poll_web_worker();
}

/// Waits only for test-owned worker completion, with a fixed timeout.
fn finish_metadata(controller: &mut AppController) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while controller.web.metadata.worker.is_some() {
        assert!(Instant::now() < deadline, "metadata worker stalled");
        controller.poll_web_worker();
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn web_metadata_worker_is_nonblocking_and_reuses_success_and_failure_cache() {
    let mut controller = metadata_controller();
    let calls = Arc::new(AtomicU64::new(0));
    let count = Arc::clone(&calls);
    controller.web.metadata.loader = Arc::new(move |url, _, _, _| {
        count.fetch_add(1, AtomicOrdering::Relaxed);
        super::super::web_metadata::LoadedWebMetadata {
            metadata: if url.path().ends_with(".opus") {
                tagged_metadata()
            } else {
                WebMediaMetadata::default()
            },
            artwork: None,
        }
    });
    controller.select_row(2);
    start_metadata_now(&mut controller);
    finish_metadata(&mut controller);
    assert_eq!(
        controller.view.details.as_ref().unwrap().title,
        "Track title"
    );
    controller.select_row(3);
    start_metadata_now(&mut controller);
    finish_metadata(&mut controller);
    for _ in 0..4 {
        controller.select_row(2);
        controller.select_row(3);
        controller.poll_web_worker();
    }
    assert_eq!(calls.load(AtomicOrdering::Relaxed), 2);
    assert!(controller.web.metadata.request.is_none());
}

#[test]
fn web_metadata_changing_selection_cancels_slow_work_and_never_starts_a_second_worker() {
    let mut controller = metadata_controller();
    let (started, observed) = bounded(1);
    let (cancelled_sender, cancelled_receiver) = bounded(1);
    controller.web.metadata.loader = Arc::new(move |_, cancelled, _, _| {
        started.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cancelled.load(AtomicOrdering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = cancelled_sender.send(cancelled.load(AtomicOrdering::Relaxed));
        super::super::web_metadata::LoadedWebMetadata {
            metadata: tagged_metadata(),
            artwork: None,
        }
    });
    controller.select_row(2);
    start_metadata_now(&mut controller);
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    // Selection must return while the worker is still alive, not join it.
    controller.select_row(3);
    assert_eq!(controller.view.details.as_ref().unwrap().title, "02.mp4");
    assert!(controller.web.metadata.worker.is_some());
    assert!(
        cancelled_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
    );
    controller.select_row(0);
    finish_metadata(&mut controller);
    assert!(
        controller.web.metadata.cache.is_empty(),
        "cancelled result must not be cached"
    );
    assert!(controller.view.details.is_none());
}

#[test]
fn web_metadata_leaving_tab_cancels_work_without_touching_new_details() {
    let mut controller = metadata_controller();
    let (started, observed) = bounded(1);
    controller.web.metadata.loader = Arc::new(move |_, cancelled, _, _| {
        started.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cancelled.load(AtomicOrdering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        super::super::web_metadata::LoadedWebMetadata {
            metadata: tagged_metadata(),
            artwork: None,
        }
    });
    controller.select_row(2);
    start_metadata_now(&mut controller);
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    controller.show_screen(Screen::Playlists);
    let details = controller.view.details.clone();
    finish_metadata(&mut controller);
    assert_eq!(controller.view.details, details);
    assert!(controller.web.metadata.cache.is_empty());
}

#[test]
fn web_metadata_refresh_invalidation_allows_retry_and_drops_cached_raw_bytes() {
    let mut controller = metadata_controller();
    controller.select_row(2);
    let url = controller.selected_web_entry().unwrap().url.clone();
    let mut metadata = tagged_metadata();
    metadata.probe_prefix = vec![0; 256];
    controller.cache_web_metadata(url, metadata, None);
    let retained = &controller
        .web
        .metadata
        .cache
        .values()
        .next()
        .unwrap()
        .result
        .metadata;
    assert!(retained.probe_prefix.is_empty());
    assert!(retained.artwork.is_none());
    controller.update_web_detail();
    assert!(controller.web.metadata.request.is_none());
    controller.invalidate_web_metadata();
    controller.update_web_detail();
    assert!(controller.web.metadata.cache.is_empty());
    assert!(controller.web.metadata.request.is_some());
    assert_eq!(controller.view.details.as_ref().unwrap().title, "01.opus");
}

#[test]
fn web_metadata_refresh_rejects_old_result_even_for_the_same_url() {
    let mut controller = metadata_controller();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&calls);
    let (started, observed) = bounded(1);
    controller.web.metadata.loader = Arc::new(move |_, cancelled, _, _| {
        let call = counter.fetch_add(1, AtomicOrdering::Relaxed);
        if call == 0 {
            started.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !cancelled.load(AtomicOrdering::Relaxed) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
        }
        let mut metadata = tagged_metadata();
        metadata.title = Some(if call == 0 { "Stale" } else { "Fresh" }.into());
        super::super::web_metadata::LoadedWebMetadata {
            metadata,
            artwork: None,
        }
    });
    controller.select_row(2);
    start_metadata_now(&mut controller);
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    controller.invalidate_web_metadata();
    controller.update_web_detail();
    finish_metadata(&mut controller);
    assert!(controller.web.metadata.cache.is_empty());
    assert_eq!(controller.view.details.as_ref().unwrap().title, "01.opus");
    start_metadata_now(&mut controller);
    finish_metadata(&mut controller);
    assert_eq!(controller.view.details.as_ref().unwrap().title, "Fresh");
    assert_eq!(calls.load(AtomicOrdering::Relaxed), 2);
}

#[test]
fn web_metadata_cached_cover_belongs_only_to_its_media_url() {
    let mut controller = metadata_controller();
    controller.select_row(2);
    let url = controller.selected_web_entry().unwrap().url.clone();
    let cover = url::Url::parse("file:///cache/opaque-cover.image").unwrap();
    controller.cache_web_metadata(url, tagged_metadata(), Some(cover.clone()));
    controller.update_web_detail();
    assert_eq!(
        controller.view.details.as_ref().unwrap().thumbnail_url,
        Some(cover)
    );
    controller.select_row(3);
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .thumbnail_url
            .is_none()
    );
}
