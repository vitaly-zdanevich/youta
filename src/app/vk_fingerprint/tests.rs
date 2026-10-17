//! Offline ownership and private-staging regressions for explicit VK identification.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Supplies selected canonical metadata and an inert latest-only worker queue.
fn controller() -> (AppController, Receiver<LocalFingerprintWorkerRequest>) {
    let mut app = AppController::new(
        Config::for_dir("/tmp/youta-vk-fingerprint-fixture"),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    app.shutdown_local_fingerprint_worker();
    app.config.providers.vk_cookies_file =
        Some(std::env::temp_dir().join("vk-identify-fixture-cookies"));
    app.config.providers.acoustid_client_key = Some("fixture-client-key".to_owned());
    let (sender, receiver) = bounded(1);
    app.local_fingerprint_requests = Some(sender);
    app.local_fingerprint_request_drain = receiver.clone();
    select(&mut app, 11);
    (app, receiver)
}

/// Changes only trusted display metadata without reading cookies or using a provider.
fn select(app: &mut AppController, number: u64) {
    let id = crate::providers::vk::VkAudioId::new(7, number).unwrap();
    app.vk = vk::VkState::export_fixture(
        crate::providers::vk::VkTrack::fixture(id, "Fixture track", "Fixture artist"),
        app.config.providers.vk_cookies_file.clone().unwrap(),
    );
    let media_id = MediaId::new(SourceKind::Vk, id.to_string());
    app.view.screen = Screen::Vk;
    app.view.selected = 0;
    app.view.rows = vec![RowView {
        media_id: Some(media_id.clone()),
        ..RowView::default()
    }];
    app.view.details = Some(DetailView {
        media_id: Some(media_id),
        webpage_url: Some(id.webpage_url()),
        title: "Fixture track".to_owned(),
        channel_name: "Fixture artist".to_owned(),
        description: "Provider metadata".to_owned(),
        links: vec![DetailLinkView {
            label: "Fixture album".to_owned(),
            url: "https://vk.ru/music/album/-2000123_456".to_owned(),
            internal_target: Some(DetailLinkInternalTarget::VkAlbum(
                "https://vk.ru/music/album/-2000123_456".to_owned(),
            )),
            ..DetailLinkView::default()
        }],
        ..DetailView::default()
    });
}

/// Returns a validated, deterministic lookup result for presentation tests.
fn candidate() -> MusicBrainzCandidate {
    MusicBrainzCandidate::new(
        "12345678-1234-4234-8234-123456789abc",
        "12345678-1234-4234-8234-123456789def",
        0.9,
    )
    .unwrap()
}

/// Selecting a track only advertises the action; cached links cannot erase album metadata.
#[test]
fn vk_fingerprint_projection_is_explicit_idempotent_and_preserves_provider_links() {
    let (mut app, requests) = controller();
    app.apply_vk_fingerprint_details();
    assert!(
        app.view
            .details
            .as_ref()
            .unwrap()
            .local_fingerprint_available
    );
    assert!(requests.is_empty());
    let selected = app.selected_vk_fingerprint_source().unwrap();
    app.vk_fingerprint
        .remember(selected.owner, vec![candidate()]);
    app.apply_vk_fingerprint_details();
    app.apply_vk_fingerprint_details();
    let details = app.view.details.as_ref().unwrap();
    assert_eq!(details.links.len(), 2);
    assert!(matches!(
        details.links[0].internal_target,
        Some(DetailLinkInternalTarget::VkAlbum(_))
    ));
    assert_eq!(details.links[1].url, candidate().url().as_str());
    assert_eq!(details.description, "Provider metadata");
    app.local_fingerprint_requests = None;
}

/// Missing keys and stale metadata must never enqueue authenticated staging.
#[test]
fn vk_fingerprint_missing_key_or_stale_selection_never_stages_audio() {
    let (mut app, requests) = controller();
    app.config.providers.acoustid_client_key = None;
    app.fingerprint_selected_local_audio();
    assert!(requests.is_empty());
    assert!(app.view.error_popup.is_some());
    app.view.rows[0].media_id = Some(MediaId::new(SourceKind::Vk, "7_12"));
    app.apply_vk_fingerprint_details();
    assert!(
        !app.view
            .details
            .as_ref()
            .unwrap()
            .local_fingerprint_available
    );
    assert!(app.selected_vk_fingerprint_source().is_none());
    app.local_fingerprint_requests = None;
}

/// New selections and cookie imports revoke result ownership before late replies arrive.
#[test]
fn vk_fingerprint_selection_session_and_hidden_tabs_reject_late_results() {
    for change in 0..3 {
        let (mut app, requests) = controller();
        app.fingerprint_selected_local_audio();
        let LocalFingerprintWorkerRequest::IdentifyVk(request) = requests.try_recv().unwrap()
        else {
            panic!("expected native VK fingerprint request");
        };
        assert!(app.view.details.as_ref().unwrap().local_fingerprint_pending);
        assert_eq!(
            request.export_source.webpage_url(),
            request.source.owner.source
        );
        match change {
            0 => select(&mut app, 12),
            1 => {
                app.config.providers.vk_cookies_file =
                    Some(std::env::temp_dir().join("vk-other-fixture-cookies"))
            }
            _ => app.view.screen = Screen::Local,
        }
        app.drain_vk_fingerprint_responses();
        assert!(request.cancellation.is_cancelled());
        request
            .responses
            .send(Completion {
                generation: request.generation,
                owner: request.source.owner.clone(),
                result: Ok(vec![candidate()]),
            })
            .unwrap();
        let details = app.view.details.clone();
        app.drain_vk_fingerprint_responses();
        assert_eq!(app.view.details, details);
        assert!(app.vk_fingerprint.results.is_empty());
        app.local_fingerprint_requests = None;
    }
}

/// Replacing an imported session clears cached results and cancels queued identification.
#[test]
fn vk_fingerprint_session_clear_cancels_and_discards_results() {
    let (mut app, requests) = controller();
    let owner = app.selected_vk_fingerprint_source().unwrap().owner;
    app.vk_fingerprint.remember(owner, vec![candidate()]);
    app.fingerprint_selected_local_audio();
    let LocalFingerprintWorkerRequest::IdentifyVk(request) = requests.try_recv().unwrap() else {
        panic!("VK request");
    };
    app.clear_vk_fingerprint_session();
    assert!(request.cancellation.is_cancelled());
    assert!(app.vk_fingerprint.pending.is_none());
    assert!(app.vk_fingerprint.results.is_empty());
    app.local_fingerprint_requests = None;
}

/// Fake fpcalc observes private, complete staging while the test never invokes a helper.
struct Identifier(Arc<AtomicUsize>);

impl AudioIdentifier for Identifier {
    fn identify(
        &mut self,
        path: &Path,
        cancellation: &AudioIdentificationCancellation,
    ) -> Result<Vec<MusicBrainzCandidate>, AudioIdentificationError> {
        assert!(!cancellation.is_cancelled());
        assert_eq!(std::fs::read(path).unwrap(), b"fixture audio");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
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
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![candidate()])
    }
}

/// Cancellation is checked before and after staging, and temporary audio always disappears.
#[test]
fn vk_fingerprint_private_staging_cleans_up_and_observes_cancellation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut identifier = Identifier(calls.clone());
    let cancellation = AudioIdentificationCancellation::new();
    let mut staged = PathBuf::new();
    let result = identify_with_staging(&cancellation, &mut identifier, |directory| {
        staged = directory.join("audio.opus");
        std::fs::write(&staged, b"fixture audio").unwrap();
        Ok(staged.clone())
    })
    .unwrap();
    assert_eq!(result, vec![candidate()]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!staged.exists());
    cancellation.cancel();
    assert!(
        identify_with_staging(&cancellation, &mut identifier, |_| panic!(
            "cancelled staging must not run"
        ))
        .is_err()
    );
    let cancellation = AudioIdentificationCancellation::new();
    assert!(
        identify_with_staging(&cancellation, &mut identifier, |directory| {
            staged = directory.join("audio.opus");
            std::fs::write(&staged, b"fixture audio").unwrap();
            cancellation.cancel();
            Ok(staged.clone())
        })
        .is_err()
    );
    assert!(!staged.exists());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
