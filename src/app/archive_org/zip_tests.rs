//! ZIP containers remain navigation rows while members retain exact remote identities.

use super::*;

/// Gates only authoritative item and ZIP-listing requests; optional HTML is unavailable.
struct ZipTransport {
    entered: Sender<url::Url>,
    release: Receiver<Result<Vec<u8>, crate::providers::ProviderError>>,
}

impl crate::providers::archive_org::ArchiveOrgTransport for ZipTransport {
    fn fetch(&self, url: &url::Url, _: usize) -> Result<Vec<u8>, crate::providers::ProviderError> {
        if !url.path().starts_with("/metadata/") && !url.path().ends_with(".zip/") {
            return Err(crate::providers::ProviderError::HttpStatus(404));
        }
        self.entered
            .send(url.clone())
            .expect("test observes request");
        self.release
            .recv_timeout(Duration::from_secs(5))
            .expect("test releases worker")
    }
}

/// Installs one deterministic provider without contacting Archive.org.
fn transport(
    controller: &mut AppController,
) -> (
    Receiver<url::Url>,
    Sender<Result<Vec<u8>, crate::providers::ProviderError>>,
) {
    let (entered, requests) = bounded(1);
    let (release, responses) = bounded(1);
    controller.archive_org.client = ArchiveOrgClient::with_transport(Arc::new(ZipTransport {
        entered,
        release: responses,
    }));
    (requests, release)
}

/// A complete Archive listing contains one nested, individually addressable audio member.
fn listing() -> Vec<u8> {
    br#"<table class="archext"><tr><td><a href="//archive.org/download/fixture/Mix%20tape.zip/Disc%201/01%20song.mp3">01 song.mp3</a><td><td><td id="size">512</tr></table>"#.to_vec()
}

/// Builds root metadata containing one normal track and a separately browsable ZIP.
fn root_details(tracks: bool) -> Arc<ArchiveOrgItemDetails> {
    let mut details = (*tests::lookup_details("fixture")).clone();
    if !tracks {
        details.tracks.clear();
    }
    details.archives = vec![ArchiveOrgZip {
        filename: "Mix tape.zip".into(),
        size_bytes: Some(1024),
    }];
    Arc::new(details)
}

/// Supplies a member snapshot without performing network or local extraction work.
fn member_details(parent: &ArchiveOrgItemDetails) -> Arc<ArchiveOrgItemDetails> {
    let mut details = parent.clone();
    let mut track = tests::lookup_details("fixture").tracks[0].clone();
    track.filename = "Mix tape.zip/Disc 1/01 song.mp3".into();
    track.title = "Disc 1/01 song.mp3".into();
    track.download_url = url::Url::parse(
        "https://archive.org/download/fixture/Mix%20tape.zip/Disc%201/01%20song.mp3",
    )
    .unwrap();
    track.download_variants = vec![ArchiveOrgDownloadVariant {
        filename: track.filename.clone(),
        download_url: track.download_url.clone(),
        format: "MP3".into(),
        size_bytes: Some(512),
        provenance: crate::providers::archive_org::ArchiveOrgFileProvenance::Unknown,
        is_video: false,
    }];
    details.tracks = vec![track];
    details.archives.clear();
    details.archive_filename = Some("Mix tape.zip".into());
    Arc::new(details)
}

/// Installs a complete root and its source catalogue before projecting rows.
fn install_root(controller: &mut AppController, details: Arc<ArchiveOrgItemDetails>) {
    controller.view.screen = Screen::ArchiveOrg;
    controller.archive_org.items = vec![details.item.clone()];
    controller.archive_org.active = Some(Arc::clone(&details));
    controller
        .archive_org
        .cache
        .push_back(("fixture".into(), Ok(details)));
    controller.populate_archive_org();
}

/// ZIP-only items and mixed items expose containers without playable identities or HTTP.
#[test]
fn zip_rows_are_lazy_nonplayable_containers_after_normal_tracks() {
    for tracks in [false, true] {
        let (_directory, mut controller) = tests::lookup_controller();
        let root = root_details(tracks);
        let zip_index = root.tracks.len();
        install_root(&mut controller, root);
        assert_eq!(controller.view.rows.len(), zip_index + 1);
        assert!(controller.view.rows[zip_index].media_id.is_none());
        assert!(
            controller.view.rows[zip_index]
                .title
                .contains("Mix tape.zip")
        );
        controller.view.selected = zip_index;
        controller.update_archive_org_detail();
        let details = controller.view.details.as_ref().unwrap();
        assert!(details.description.contains("Mix tape.zip"));
        assert!(controller.selected_archive_org_queue_item().is_err());
        assert!(
            controller
                .selected_archive_org_playlist_identity()
                .is_none()
        );
        assert!(controller.archive_org.worker.is_none());
        assert!(controller.archive_org.pending.is_none());
    }
}

/// Opening cached members preserves the enclosing item cache and Back restores its ZIP row.
#[test]
fn zip_members_keep_parent_cache_queue_identity_and_audio_only_autoplay() {
    let (_directory, mut controller) = tests::lookup_controller();
    let root = root_details(true);
    let members = member_details(&root);
    install_root(&mut controller, Arc::clone(&root));
    controller.cache_archive_response("fixture", Some("Mix tape.zip"), Ok(Arc::clone(&members)));
    controller.view.selected = root.tracks.len();
    controller.activate_archive_org_selection();
    assert_eq!(
        controller
            .archive_org
            .active
            .as_ref()
            .unwrap()
            .archive_filename
            .as_deref(),
        Some("Mix tape.zip")
    );
    assert!(Arc::ptr_eq(
        &controller
            .cached_archive_details("fixture")
            .unwrap()
            .unwrap(),
        &root
    ));
    let queued = controller.selected_archive_org_queue_item().unwrap();
    assert_eq!(queued.media.id.source, SourceKind::ArchiveOrg);
    assert_eq!(
        queued.media.id.external_id,
        members.tracks[0].download_url.as_str()
    );
    assert_eq!(queued.playback_location, queued.media.id.external_id);
    controller.archive_org.cache.clear();
    assert!(
        controller
            .archive_download_variants(&members.tracks[0].download_url)
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        playback_step(
            &members,
            0,
            crate::config::ArchivePlaybackPreference::AudioOnly
        ),
        AutoplayStep::Play { .. }
    ));
    assert!(controller.go_back_archive_org());
    assert!(Arc::ptr_eq(
        controller.archive_org.active.as_ref().unwrap(),
        &root
    ));
    assert_eq!(controller.view.selected, root.tracks.len());
    assert!(controller.archive_org.worker.is_none());
}

/// A cancelled ZIP response warms only its own cache and never opens another route.
#[test]
fn cancelled_zip_open_returns_to_parent_and_late_response_stays_passive() {
    let (_directory, mut controller) = tests::lookup_controller();
    let root = root_details(false);
    let members = member_details(&root);
    install_root(&mut controller, Arc::clone(&root));
    let job = ArchiveJob {
        generation: 7,
        kind: ArchiveRequest::Zip {
            parent: Arc::clone(&root),
            archive: root.archives[0].clone(),
            open: true,
        },
        due: Instant::now(),
    };
    controller.archive_org.generation = 7;
    controller.archive_org.pending = Some(job.clone());
    assert!(controller.go_back_archive_org());
    assert!(Arc::ptr_eq(
        controller.archive_org.active.as_ref().unwrap(),
        &root
    ));
    controller.handle_archive_response(job, Ok(ArchiveResponse::Details(Arc::clone(&members))));
    assert!(Arc::ptr_eq(
        controller.archive_org.active.as_ref().unwrap(),
        &root
    ));
    assert!(Arc::ptr_eq(
        &controller
            .cached_archive_details_for("fixture", Some("Mix tape.zip"))
            .unwrap()
            .unwrap(),
        &members
    ));
    assert!(Arc::ptr_eq(
        &controller
            .cached_archive_details("fixture")
            .unwrap()
            .unwrap(),
        &root
    ));
}

/// Back can cancel parent recovery after an open ZIP outlives its root metadata.
#[test]
fn second_back_cancels_zip_parent_recovery_without_reopening_the_item() {
    let (_directory, mut controller) = tests::lookup_controller();
    let root = root_details(false);
    let members = member_details(&root);
    install_root(&mut controller, Arc::clone(&root));
    controller.archive_org.active = Some(members);
    controller.archive_org.cache.clear();
    controller.archive_org.zip_parent = None;
    controller.populate_archive_org();
    let (requests, release) = transport(&mut controller);

    assert!(controller.go_back_archive_org());
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .path(),
        "/metadata/fixture"
    );
    assert_eq!(
        controller.archive_org.zip_return_filename.as_deref(),
        Some("Mix tape.zip")
    );
    let canceled = controller.archive_org.pending.as_ref().unwrap().clone();

    assert!(controller.go_back_archive_org());
    // Release the original worker even when this regression fails below.
    release
        .send(Err(crate::providers::ProviderError::HttpStatus(503)))
        .unwrap();
    assert!(controller.archive_org.active.is_none());
    assert!(controller.archive_org.zip_return_filename.is_none());
    assert!(
        controller
            .archive_org
            .pending
            .as_ref()
            .is_none_or(|job| { matches!(job.kind, ArchiveRequest::Details { open: false, .. }) })
    );

    // A stale successful response may warm the cache, but must not reopen the item.
    controller.handle_archive_response(canceled, Ok(ArchiveResponse::Details(root)));
    assert!(controller.archive_org.active.is_none());
    assert!(controller.archive_org.zip_return_filename.is_none());
    assert_eq!(controller.view.rows.len(), 1);
    assert!(controller.view.rows[0].media_id.is_none());
}

/// Enter coalesces repeated opens; leaving the ZIP row keeps the result cache-only.
#[test]
fn zip_listing_worker_is_lazy_coalesced_and_selection_owned() {
    let (_directory, mut controller) = tests::lookup_controller();
    let root = root_details(true);
    install_root(&mut controller, Arc::clone(&root));
    let (requests, release) = transport(&mut controller);
    controller.view.selected = root.tracks.len();
    controller.update_archive_org_detail();
    assert!(requests.try_recv().is_err());
    controller.activate_archive_org_selection();
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .path(),
        "/download/fixture/Mix%20tape.zip/"
    );
    let generation = controller.archive_org.generation;
    controller.activate_archive_org_selection();
    assert_eq!(controller.archive_org.generation, generation);
    controller.view.selected = 0;
    controller.update_archive_org_detail();
    assert!(controller.archive_org.pending.is_none());
    release.send(Ok(listing())).unwrap();
    tests::finish_lookup_worker(&mut controller);
    assert!(Arc::ptr_eq(
        controller.archive_org.active.as_ref().unwrap(),
        &root
    ));
    assert!(
        controller
            .cached_archive_details_for("fixture", Some("Mix tape.zip"))
            .unwrap()
            .is_ok()
    );
    assert!(requests.try_recv().is_err());
    controller.view.selected = root.tracks.len();
    controller.activate_archive_org_selection();
    assert_eq!(controller.view.rows.len(), 1);
    assert!(controller.archive_org.worker.is_none());
}

/// Failed ZIP metadata stays inert until a new explicit activation retries it.
#[test]
fn zip_listing_failure_is_retryable_without_automatic_requests() {
    let (_directory, mut controller) = tests::lookup_controller();
    install_root(&mut controller, root_details(false));
    let (requests, release) = transport(&mut controller);
    controller.activate_archive_org_selection();
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    release
        .send(Err(crate::providers::ProviderError::HttpStatus(503)))
        .unwrap();
    tests::finish_lookup_worker(&mut controller);
    assert!(controller.view.error_popup.is_some());
    for _ in 0..3 {
        controller.poll_archive_org_worker();
    }
    assert!(requests.try_recv().is_err());
    assert!(
        controller
            .archive_org
            .active
            .as_ref()
            .unwrap()
            .archive_filename
            .is_none()
    );
    controller.view.error_popup = None;
    controller.activate_archive_org_selection();
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    release.send(Ok(listing())).unwrap();
    tests::finish_lookup_worker(&mut controller);
    assert_eq!(
        controller
            .archive_org
            .active
            .as_ref()
            .unwrap()
            .archive_filename
            .as_deref(),
        Some("Mix tape.zip")
    );
}

/// Fresh member downloads resolve parent and listing without replacing the visible screen.
#[test]
fn zip_member_download_lookup_stages_parent_then_zip_without_navigation() {
    let (_directory, mut controller) = tests::lookup_controller();
    let (requests, release) = transport(&mut controller);
    let source = member_details(&root_details(false)).tracks[0]
        .download_url
        .clone();
    assert!(
        controller
            .archive_download_variants(&source)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .path(),
        "/metadata/fixture"
    );
    release
        .send(Ok(serde_json::to_vec(&serde_json::json!({
            "metadata": {"identifier":"fixture","title":"Fixture","mediatype":"audio"},
            "files": [{"name":"Mix tape.zip","format":"ZIP","source":"original","size":"1024"}]
        }))
        .unwrap()))
        .unwrap();
    tests::finish_lookup_worker(&mut controller);
    assert!(
        controller
            .archive_download_variants(&source)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .path(),
        "/download/fixture/Mix%20tape.zip/"
    );
    release.send(Ok(listing())).unwrap();
    tests::finish_lookup_worker(&mut controller);
    let variants = controller
        .archive_download_variants(&source)
        .unwrap()
        .unwrap();
    assert_eq!(variants.len(), 1);
    assert_eq!(variants[0].download_url, source);
    assert_eq!(controller.view.screen, Screen::History);
    assert!(controller.archive_org.active.is_none());
    assert!(
        controller
            .cached_archive_details("fixture")
            .unwrap()
            .unwrap()
            .archive_filename
            .is_none()
    );
    assert!(requests.try_recv().is_err());
}

/// Cancelling a pinned member lookup revokes ownership while retaining passive cache data.
#[test]
fn zip_member_download_cancellation_cannot_open_its_late_listing() {
    let (_directory, mut controller) = tests::lookup_controller();
    let root = root_details(false);
    let source = member_details(&root).tracks[0].download_url.clone();
    controller.cache_archive_response("fixture", None, Ok(root));
    let (requests, release) = transport(&mut controller);
    assert!(
        controller
            .archive_download_variants(&source)
            .unwrap()
            .is_none()
    );
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    controller.cancel_archive_download_lookup();
    release.send(Ok(listing())).unwrap();
    tests::finish_lookup_worker(&mut controller);
    assert!(controller.archive_org.download_lookup.is_none());
    assert!(controller.archive_org.active.is_none());
    assert_eq!(controller.view.screen, Screen::History);
    assert!(
        controller
            .cached_archive_details_for("fixture", Some("Mix tape.zip"))
            .unwrap()
            .is_ok()
    );
    assert!(requests.try_recv().is_err());
}

/// Missing members and failed listings remain terminal for one explicit download attempt.
#[test]
fn zip_member_download_failure_or_missing_file_does_not_retry_or_substitute() {
    for failed in [false, true] {
        let (_directory, mut controller) = tests::lookup_controller();
        controller.cache_archive_response("fixture", None, Ok(root_details(false)));
        let source =
            url::Url::parse("https://archive.org/download/fixture/Mix%20tape.zip/missing.mp3")
                .unwrap();
        let (requests, release) = transport(&mut controller);
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_none()
        );
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        release
            .send(if failed {
                Err(crate::providers::ProviderError::HttpStatus(503))
            } else {
                Ok(listing())
            })
            .unwrap();
        tests::finish_lookup_worker(&mut controller);
        for _ in 0..3 {
            let error = controller.archive_download_variants(&source).unwrap_err();
            assert!(error.contains(if failed {
                "503"
            } else {
                "no available download variants"
            }));
            controller.poll_archive_org_worker();
        }
        assert!(requests.try_recv().is_err());
        assert!(controller.archive_org.active.is_none());
    }
}
