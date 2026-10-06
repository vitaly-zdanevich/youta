//! Restart restoration uses persistent state and bounded fixture metadata, never live HTTP.

use std::sync::Mutex;

use super::*;
use crate::providers::ProviderError;
use crate::providers::archive_org::ArchiveOrgTransport;

/// Serves root metadata and a ZIP directory independently without transferring ZIP bytes.
struct ZipRestartTransport {
    requests: Mutex<Vec<url::Url>>,
    missing_zip: bool,
    missing_member: bool,
}

impl ArchiveOrgTransport for ZipRestartTransport {
    fn fetch(&self, url: &url::Url, _: usize) -> Result<Vec<u8>, ProviderError> {
        self.requests.lock().unwrap().push(url.clone());
        match url.path() {
            "/advancedsearch.php" => Ok(serde_json::to_vec(&serde_json::json!({
                "response": {"numFound": 1, "start": 0, "docs": [
                    {"identifier":"second", "title":"ZIP item", "mediatype":"audio"}
                ]}
            }))
            .unwrap()),
            "/metadata/second" => {
                let mut files =
                    vec![serde_json::json!({"name":"direct.opus", "source":"original"})];
                if !self.missing_zip {
                    files.push(serde_json::json!({"name":"Album.zip", "format":"ZIP", "source":"original", "size":"9000"}));
                }
                Ok(serde_json::to_vec(&serde_json::json!({
                    "metadata": {"identifier":"second", "title":"ZIP item", "mediatype":"audio"},
                    "files": files
                }))
                .unwrap())
            }
            "/details/second" => Ok(Vec::new()),
            "/download/second/Album.zip/" => Ok(if self.missing_member {
                br#"<table class="archext"><caption>listing of Album.zip</caption><tr><td><a href="/download/second/Album.zip/01-new.mp3">01-new.mp3</a><td><td><td id="size">10</tr></table>"#.to_vec()
            } else {
                br#"<table class="archext"><caption>listing of Album.zip</caption>
                <tr><th>file<th>as jpg<th>timestamp<th>size</tr>
                <tr><td><a href="/download/second/Album.zip/01-new.mp3">01-new.mp3</a><td><td>2026-10-06<td id="size">10</tr>
                <tr><td><a href="/download/second/Album.zip/disc%2F03-selected.mp3">disc/03-selected.mp3</a><td><td>2026-10-06<td id="size">20</tr>
                </table>"#.to_vec()
            }),
            _ => panic!("unexpected ZIP restart fetch: {url}"),
        }
    }
}

/// Restart resolves the enclosing ZIP lazily and keeps Back at its original item row.
#[test]
fn archive_zip_restart_reopens_exact_member_without_playback_or_archive_download() {
    let (_temporary, config) = saved_item();
    let store = StateStore::open(&config).unwrap();
    let mut saved = store.session().unwrap().unwrap();
    let location = saved.archive_org_location.as_mut().unwrap();
    location.archive_filename = Some("Album.zip".into());
    location.filename = Some("Album.zip/disc/03-selected.mp3".into());
    store.save_session(&saved, 2).unwrap();
    drop(store);
    let (mut app, _) = reopened(config, false);
    let transport = Arc::new(ZipRestartTransport {
        requests: Mutex::new(Vec::new()),
        missing_zip: false,
        missing_member: false,
    });
    app.archive_org.client = ArchiveOrgClient::with_transport(transport.clone());
    finish_restore(&mut app);
    assert_eq!(
        app.archive_org
            .active
            .as_ref()
            .unwrap()
            .archive_filename
            .as_deref(),
        Some("Album.zip")
    );
    assert_eq!(app.view.selected, 1);
    assert_eq!(
        app.selected_archive_org_queue_item()
            .unwrap()
            .playback_location,
        "https://archive.org/download/second/Album.zip/disc/03-selected.mp3"
    );
    assert_eq!(
        app.archive_org_session_location()
            .unwrap()
            .archive_filename
            .as_deref(),
        Some("Album.zip")
    );
    assert!(app.player.is_none());
    assert!(app.playback_queue.items.is_empty());
    assert!(app.go_back_archive_org());
    assert!(
        app.archive_org
            .active
            .as_ref()
            .unwrap()
            .archive_filename
            .is_none()
    );
    assert_eq!(app.selected_archive_zip().unwrap().filename, "Album.zip");
    assert!(app.selected_archive_org_queue_item().is_err());
    assert!(app.go_back_archive_org());
    assert!(app.archive_org.active.is_none());
    assert_eq!(
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.path().ends_with(".zip/"))
            .count(),
        1
    );
}

/// Removed ZIPs or members fall back without starting playback or retrying each frame.
#[test]
fn archive_zip_restart_missing_container_or_member_stops_at_safe_parent() {
    for missing_zip in [true, false] {
        let (_temporary, config) = saved_item();
        let store = StateStore::open(&config).unwrap();
        let mut saved = store.session().unwrap().unwrap();
        let location = saved.archive_org_location.as_mut().unwrap();
        location.archive_filename = Some("Album.zip".into());
        location.filename = Some("Album.zip/disc/03-selected.mp3".into());
        store.save_session(&saved, 2).unwrap();
        drop(store);
        let (mut app, _) = reopened(config, false);
        let transport = Arc::new(ZipRestartTransport {
            requests: Mutex::new(Vec::new()),
            missing_zip,
            missing_member: !missing_zip,
        });
        app.archive_org.client = ArchiveOrgClient::with_transport(transport.clone());
        finish_restore(&mut app);
        assert!(
            app.view
                .status_line
                .contains("saved file is no longer available")
        );
        assert_eq!(app.view.selected, 0);
        assert_eq!(
            app.archive_org
                .active
                .as_ref()
                .unwrap()
                .archive_filename
                .is_none(),
            missing_zip
        );
        assert!(app.player.is_none());
        assert!(app.playback_queue.items.is_empty());
        let requests = transport.requests.lock().unwrap().len();
        for _ in 0..3 {
            app.poll_archive_org_worker();
        }
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            requests,
            "missing route must not retry each frame"
        );
    }
}

/// Revealing a fresh ZIP playback identity loads root and members without replaying.
#[test]
fn archive_zip_now_playing_reveal_resolves_member_without_changing_queue() {
    let (_temporary, mut app) = super::tests::lookup_controller();
    disable_external_enrichment(&mut app);
    app.view.screen = Screen::SoundCloud;
    app.archive_org.initialized = true;
    let transport = Arc::new(ZipRestartTransport {
        requests: Mutex::new(Vec::new()),
        missing_zip: false,
        missing_member: false,
    });
    app.archive_org.client = ArchiveOrgClient::with_transport(transport.clone());
    let source = "https://archive.org/download/second/Album.zip/disc/03-selected.mp3";
    let id = MediaId::new(SourceKind::ArchiveOrg, source);
    app.current_media = Some(id.clone());
    assert!(app.request_playing_archive_org(&id));
    finish_restore(&mut app);
    assert_eq!(app.view.screen, Screen::ArchiveOrg);
    assert_eq!(app.view.selected, 1);
    assert_eq!(app.selected_archive_org_queue_item().unwrap().media.id, id);
    assert!(app.playback_queue.items.is_empty());
    assert!(app.player.is_none());
    assert_eq!(transport.requests.lock().unwrap().len(), 3);
    assert!(app.go_back_archive_org());
    assert_eq!(app.selected_archive_zip().unwrap().filename, "Album.zip");
}

/// Returns reordered catalogue/files, recording the exact requests made after restart.
struct RestartTransport {
    requests: Mutex<Vec<url::Url>>,
    missing_file: bool,
    missing_item: bool,
}

impl ArchiveOrgTransport for RestartTransport {
    fn fetch(&self, url: &url::Url, _: usize) -> Result<Vec<u8>, ProviderError> {
        self.requests.lock().unwrap().push(url.clone());
        let value = match url.path() {
            "/advancedsearch.php" => serde_json::json!({
                "response": {
                    "numFound": 2, "start": 0,
                    "docs": [
                        {"identifier": "second", "title": "Second item", "mediatype": "audio"},
                        {"identifier": "first", "title": "First item", "mediatype": "audio"}
                    ]
                }
            }),
            "/metadata/second" => {
                if self.missing_item {
                    return Err(ProviderError::HttpStatus(404));
                }
                let mut files = vec![serde_json::json!({
                    "name": "01-inserted.opus", "title": "New first file",
                    "format": "Opus", "source": "original", "track": "1"
                })];
                if !self.missing_file {
                    files.push(serde_json::json!({
                        "name": "folder/selected recording.opus", "title": "Selected file",
                        "format": "Opus", "source": "original", "track": "2"
                    }));
                }
                serde_json::json!({
                    "metadata": {"identifier": "second", "title": "Second item", "mediatype": "audio"},
                    "files": files
                })
            }
            "/details/second" => return Ok(Vec::new()),
            _ => return Err(ProviderError::HttpStatus(404)),
        };
        Ok(serde_json::to_vec(&value).unwrap())
    }
}

/// Replaces optional public enrichment with an offline sink before projecting fixture metadata.
fn disable_external_enrichment(app: &mut AppController) {
    if let Some(sender) = app.provider_requests.take() {
        sender.send(ProviderRequest::Shutdown).unwrap();
    }
    if let Some(worker) = app.provider_thread.take() {
        worker.join().unwrap();
    }
    let (sender, requests) = unbounded();
    app.provider_requests = Some(sender);
    app.provider_thread = Some(thread::spawn(move || {
        while let Ok(request) = requests.recv() {
            if matches!(request, ProviderRequest::Shutdown) {
                break;
            }
        }
    }));
}

/// Saves a controller closed inside an item whose file ordering changes on restart.
fn saved_item() -> (tempfile::TempDir, Config) {
    let temporary = crate::test_support::canonical_tempdir("archive restart");
    let mut config = Config::for_dir(temporary.path().join("config"));
    config.persistence.backend = crate::config::PersistenceBackend::Files;
    let store = StateStore::open(&config).unwrap();
    let mut app = AppController::new(config.clone(), store, None, None);
    disable_external_enrichment(&mut app);
    app.view.screen = Screen::ArchiveOrg;
    app.archive_org.initialized = true;
    app.archive_org_search_query = "Field recording".to_owned();
    app.archive_org_search_scope = ArchiveOrgSearchScope::Topic;
    app.archive_org.submitted_query = "Field recording".to_owned();
    app.archive_org.submitted_scope = ArchiveOrgSearchScope::Topic;
    let details: ArchiveOrgItemDetails = serde_json::from_value(serde_json::json!({
        "item": {
            "identifier": "second", "title": "Second item",
            "webpage_url": "https://archive.org/details/second",
            "collections": [], "topics": [], "languages": []
        },
        "tracks": [{
            "filename": "folder/selected recording.opus", "title": "Selected file",
            "download_url": "https://archive.org/download/second/folder/selected%20recording.opus"
        }],
        "comments": []
    }))
    .unwrap();
    let mut first = details.item.clone();
    first.identifier = "first".to_owned();
    first.webpage_url = url::Url::parse("https://archive.org/details/first").unwrap();
    app.archive_org.items = vec![first, details.item.clone()];
    app.archive_org.total = 2;
    app.archive_org.search_selected = 1;
    app.archive_org.active = Some(Arc::new(details));
    app.archive_org_selected = 0;
    app.populate_archive_org();
    app.shutdown_for_exit().unwrap();
    drop(app);
    (temporary, config)
}

/// Opens the real saved backend and injects a mock before the first lazy metadata request.
fn reopened(config: Config, missing_file: bool) -> (AppController, Arc<RestartTransport>) {
    let store = StateStore::open(&config).unwrap();
    let mut app = AppController::new(config, store, None, None);
    disable_external_enrichment(&mut app);
    app.playback_factory = Some(Box::new(|| {
        panic!("Archive metadata restoration must not initialize playback")
    }));
    assert_eq!(app.view.screen, Screen::ArchiveOrg);
    assert!(
        app.archive_org.worker.is_none(),
        "construction must not fetch"
    );
    assert!(app.archive_org.pending.is_none());
    let transport = Arc::new(RestartTransport {
        requests: Mutex::new(Vec::new()),
        missing_file,
        missing_item: false,
    });
    app.archive_org.client = ArchiveOrgClient::with_transport(transport.clone());
    (app, transport)
}

/// Completes only fixture-backed work, including queued restoration stages.
fn finish_restore(app: &mut AppController) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.poll_archive_org_worker();
        if app.archive_org.worker.is_none() && app.archive_org.request.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bounded restart metadata completed"
        );
        thread::yield_now();
    }
}

/// A real close/reopen selects the exact file after both catalogue and track reordering.
#[test]
fn archive_session_restart_reopens_exact_file_without_playback() {
    let (_temporary, config) = saved_item();
    let (mut app, transport) = reopened(config, false);
    app.set_archive_org_search_page_capacity(8);
    finish_restore(&mut app);
    assert_eq!(
        app.archive_org.active.as_ref().unwrap().item.identifier,
        "second"
    );
    assert_eq!(
        app.view.selected, 1,
        "file identity must win over its old row zero"
    );
    assert_eq!(
        app.selected_archive_org_queue_item()
            .unwrap()
            .playback_location,
        "https://archive.org/download/second/folder/selected%20recording.opus"
    );
    assert!(app.playback_queue.items.is_empty());
    assert!(app.player.is_none());
    assert!(app.go_back_archive_org());
    assert!(app.archive_org.active.is_none());
    assert_eq!(
        app.view.selected, 0,
        "catalogue identity must win over its old row one"
    );
    assert_eq!(
        app.archive_org.items[app.view.selected].identifier,
        "second"
    );
    assert_eq!(app.archive_org.submitted_query, "Field recording");
    assert_eq!(
        app.archive_org.submitted_scope,
        ArchiveOrgSearchScope::Topic
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|url| url.path() == "/metadata/second")
            .count(),
        1
    );
    assert!(
        requests.len() <= 3,
        "one catalogue, one metadata, optional public-page enrichment"
    );
}

/// A removed file keeps the item open with an explicit fallback and never starts audio.
#[test]
fn archive_session_restart_missing_file_explains_fallback_without_playback() {
    let (_temporary, config) = saved_item();
    let (mut app, _transport) = reopened(config, true);
    finish_restore(&mut app);
    assert_eq!(
        app.archive_org.active.as_ref().unwrap().item.identifier,
        "second"
    );
    assert_eq!(app.view.selected, 0);
    assert!(
        app.view
            .status_line
            .contains("saved file is no longer available")
    );
    assert!(app.playback_queue.items.is_empty());
    assert!(app.player.is_none());
}

/// Periodic saves before and during metadata loading retain the logical destination.
#[test]
fn archive_session_restart_pending_save_keeps_exact_filename_and_submitted_query() {
    let (_temporary, config) = saved_item();
    let store = StateStore::open(&config).unwrap();
    let mut saved = store.session().unwrap().unwrap();
    let location = saved.archive_org_location.clone().unwrap();
    saved.archive_org_search_text = "unsubmitted editor draft".to_owned();
    store.save_session(&saved, 1).unwrap();
    drop(store);
    let (mut app, _transport) = reopened(config, false);
    assert!(app.save_session());
    assert_eq!(
        app.store.session().unwrap().unwrap().archive_org_location,
        Some(location.clone())
    );
    assert_eq!(
        app.store
            .session()
            .unwrap()
            .unwrap()
            .archive_org_selected_row,
        Some(1)
    );

    super::tests::occupy_archive_worker(&mut app);
    app.populate_archive_org();
    assert!(app.archive_org.restoring.is_some());
    assert!(app.archive_org.active.is_none());
    assert!(app.save_session());
    assert_eq!(
        app.store.session().unwrap().unwrap().archive_org_location,
        Some(location)
    );
    assert_eq!(
        app.store
            .session()
            .unwrap()
            .unwrap()
            .archive_org_selected_row,
        Some(1),
        "loading rows must not overwrite the saved parent catalogue selection"
    );
    assert_eq!(app.archive_org.submitted_query, "Field recording");
    assert_eq!(
        app.archive_org.submitted_scope,
        ArchiveOrgSearchScope::Topic
    );
    app.archive_org
        .worker
        .take()
        .unwrap()
        .thread
        .join()
        .unwrap();
    finish_restore(&mut app);
    assert_eq!(
        app.archive_org.active.as_ref().unwrap().tracks[app.view.selected].filename,
        "folder/selected recording.opus"
    );
    assert!(app.playback_queue.items.is_empty());
}

/// A deleted or inaccessible item falls back once, without modal interruption or retry loops.
#[test]
fn archive_session_restart_missing_item_returns_to_parent_without_retrying() {
    let (_temporary, config) = saved_item();
    let (mut app, _transport) = reopened(config, false);
    let transport = Arc::new(RestartTransport {
        requests: Mutex::new(Vec::new()),
        missing_file: false,
        missing_item: true,
    });
    app.archive_org.client = ArchiveOrgClient::with_transport(transport.clone());
    finish_restore(&mut app);
    assert!(app.archive_org.active.is_none());
    assert!(app.archive_org.restoring.is_none());
    assert_eq!(
        app.archive_org.items[app.view.selected].identifier,
        "second"
    );
    assert!(
        app.view
            .status_line
            .contains("Saved archive.org location is unavailable")
    );
    assert!(app.view.error_popup.is_none());
    for _ in 0..3 {
        app.poll_archive_org_worker();
        app.populate_archive_org();
    }
    assert_eq!(
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.path() == "/metadata/second")
            .count(),
        1
    );
    assert!(app.playback_queue.items.is_empty());
    assert!(app.player.is_none());
}

/// Invalid persisted paths are ignored before a provider request can use them.
#[test]
fn archive_session_restart_ignores_invalid_saved_filename() {
    let (_temporary, config) = saved_item();
    let store = StateStore::open(&config).unwrap();
    let mut saved = store.session().unwrap().unwrap();
    saved.archive_org_location.as_mut().unwrap().filename = Some("../private.opus".to_owned());
    store.save_session(&saved, 1).unwrap();
    drop(store);
    let (mut app, transport) = reopened(config, false);
    assert!(app.archive_org.restart.is_none());
    finish_restore(&mut app);
    assert!(app.archive_org.active.is_none());
    assert!(app.playback_queue.items.is_empty());
    assert!(transport.requests.lock().unwrap().iter().all(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("archive.org")
            && !url.as_str().contains("private")
    }));
}
