//! Offline owner, privacy, paging, and playback tests for Funkwhale integration.

use super::*;
use crate::providers::{SearchPage, Thumbnail};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Owns a bounded local pod serving real catalogue and selected-track requests.
struct RecentPodFixture {
    base: url::Url,
    requests: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RecentPodFixture {
    /// Serves twenty public tracks, or one temporary catalogue failure.
    fn new(fail_catalogue: bool) -> Self {
        Self::start(fail_catalogue, false)
    }

    /// Serves exact artist album and standalone-track routes with API-version fallback.
    fn artist() -> Self {
        Self::start(false, true)
    }

    /// Shares bounded transport plumbing while keeping catalogue fixtures isolated.
    fn start(fail_catalogue: bool, artist_catalogue: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local pod listener");
        let address = listener.local_addr().expect("local pod address");
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&requests);
        let stopping = Arc::clone(&stopped);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stopping.load(Ordering::Relaxed) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(socket) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("local pod accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    let mut byte = [0_u8; 1];
                    if socket.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let target = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                observed.lock().unwrap().push(target.to_owned());
                let path = target.split('?').next().unwrap();
                let track = |id| {
                    let mut track = serde_json::json!({
                        "id": id,
                        "title": format!("Recent track {id}"),
                        "artist": {"id": 7, "name": "Pod artist"},
                        "creation_date": "2026-10-15T10:00:00Z",
                        "is_playable": true,
                        "listen_url": format!("http://{address}/audio/{id}.opus?version={}", observed.lock().unwrap().len()),
                        "uploads": [{"duration": 120}],
                    });
                    if id == 1 {
                        track["license"] = serde_json::json!("cc-by-4.0");
                        track["listenings_count"] = serde_json::json!(7);
                        track["downloads_count"] = serde_json::json!(999);
                    }
                    track
                };
                let artist_response = || {
                    let url = url::Url::parse(&format!("http://{address}{target}")).unwrap();
                    let query = url.query_pairs().collect::<HashMap<_, _>>();
                    match path {
                        "/api/v1/albums/" => ("404 Not Found", "{}".to_owned()),
                        "/api/v2/albums/" => {
                            let artist = query.get("artist").map(|value| value.as_ref());
                            let page = query.get("page").map(|value| value.as_ref());
                            let results = if matches!(artist, Some("5796" | "5797" | "5798")) {
                                let (id, title) = if page == Some("2") {
                                    (4409, "Second album")
                                } else {
                                    (4408, "ADL")
                                };
                                vec![
                                    serde_json::json!({"id": id, "title": title, "tracks_count": 22, "is_playable": "true"}),
                                ]
                            } else {
                                Vec::new()
                            };
                            ("200 OK", serde_json::json!({
                                "next": if matches!(artist, Some("5796" | "5797" | "5798")) && page != Some("2") { Some(format!("http://{address}/api/v2/albums/?artist={}&page=2", artist.unwrap())) } else { None },
                                "results": results,
                            }).to_string())
                        }
                        "/api/v2/tracks/" => {
                            if query.get("page_size").is_some_and(|value| value == "1")
                                && query
                                    .get("ordering")
                                    .is_some_and(|value| value == "-album__title")
                            {
                                let artist = query.get("artist").map(|value| value.as_ref());
                                if artist == Some("5798") {
                                    return ("503 Service Unavailable", "{}".to_owned());
                                }
                                let results = if artist == Some("5795") {
                                    Vec::new()
                                } else {
                                    let mut value = track(27084);
                                    value["album"] = if artist == Some("5796") {
                                        serde_json::json!({"id": 4408, "title": "ADL"})
                                    } else {
                                        serde_json::Value::Null
                                    };
                                    vec![value]
                                };
                                return (
                                    "200 OK",
                                    serde_json::json!({"results": results}).to_string(),
                                );
                            }
                            let (results, next) =
                                if query.get("album").is_some_and(|value| value == "4408") {
                                    (
                                    (1..=3)
                                        .map(|position| {
                                            let mut value = track(27107 + position);
                                            value["artist"] =
                                                serde_json::json!({"id": 5796, "name": "Allan"});
                                            value["album"] =
                                                serde_json::json!({"id": 4408, "title": "ADL"});
                                            value["disc_number"] =
                                                serde_json::json!(if position == 3 {
                                                    2
                                                } else {
                                                    1
                                                });
                                            value["position"] =
                                                serde_json::json!(if position == 2 {
                                                    2
                                                } else {
                                                    1
                                                });
                                            value
                                        })
                                        .collect::<Vec<_>>(),
                                    None,
                                )
                                } else if query.get("artist").is_some_and(|value| value == "5794") {
                                    let mut bound = track(27083);
                                    bound["album"] =
                                        serde_json::json!({"id": 999, "title": "Not standalone"});
                                    let values = [bound, track(27085), track(27084)]
                                        .into_iter()
                                        .map(|mut value| {
                                            value["artist"] =
                                                serde_json::json!({"id": 5794, "name": "Vortex"});
                                            value
                                        })
                                        .collect::<Vec<_>>();
                                    (
                                        values,
                                        Some(format!(
                                            "http://{address}/api/v2/tracks/?artist=5794&page=2"
                                        )),
                                    )
                                } else if query.get("artist").is_some_and(|value| value == "5797") {
                                    let later_page =
                                        query.get("page").is_some_and(|value| value == "2");
                                    let mut value = track(if later_page { 27085 } else { 27083 });
                                    if !later_page {
                                        value["album"] =
                                            serde_json::json!({"id": 4408, "title": "ADL"});
                                    }
                                    (
                                        vec![value],
                                        (!later_page).then(|| {
                                            format!(
                                                "http://{address}/api/v2/tracks/?artist=5797&page=2"
                                            )
                                        }),
                                    )
                                } else {
                                    (Vec::new(), None)
                                };
                            (
                                "200 OK",
                                serde_json::json!({"results": results, "next": next}).to_string(),
                            )
                        }
                        "/api/v2/tracks/27108/" | "/api/v2/tracks/27085/" => {
                            let id = path
                                .trim_end_matches('/')
                                .rsplit('/')
                                .next()
                                .unwrap()
                                .parse::<u32>()
                                .unwrap();
                            let mut value = track(id);
                            value["artist"] = if id == 27108 {
                                serde_json::json!({"id": 5796, "name": "Allan"})
                            } else {
                                serde_json::json!({"id": 5794, "name": "Vortex"})
                            };
                            ("200 OK", value.to_string())
                        }
                        _ => ("404 Not Found", "{}".to_owned()),
                    }
                };
                let (status, body) = if artist_catalogue {
                    artist_response()
                } else {
                    match path {
                        "/api/v1/tracks/" if fail_catalogue => {
                            ("503 Service Unavailable", "{}".to_owned())
                        }
                        "/api/v1/tracks/" => (
                            "200 OK",
                            serde_json::json!({
                                "count": 40,
                                "next": format!("http://{address}/api/v1/tracks/?page=2"),
                                "results": (1..=20).map(track).collect::<Vec<_>>(),
                            })
                            .to_string(),
                        ),
                        "/api/v1/tracks/1/" => ("200 OK", track(1).to_string()),
                        _ => ("404 Not Found", "{}".to_owned()),
                    }
                };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            base: url::Url::parse(&format!("http://{address}/")).unwrap(),
            requests,
            stopped,
            thread: Some(thread),
        }
    }

    /// Copies request targets without exposing unrelated headers or credentials.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for RecentPodFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.expect("local pod fixture");
            }
        }
    }
}

/// Drains the production worker until all expected UI work finishes, with a finite deadline.
fn finish_recent_requests(app: &mut AppController, ready: impl Fn(&AppController) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.poll_funkwhale_worker();
        if ready(app) && app.funkwhale.worker.is_none() && app.funkwhale.jobs.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "recent pod request did not finish"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

/// Creates a normal controller whose only configured network source is the local pod.
fn recent_controller(pod: &RecentPodFixture, history: bool) -> AppController {
    let mut config = Config::for_dir("/tmp/youta-funkwhale-recent-http-fixture");
    config.providers.funkwhale_instance_url = Some(pod.base.clone());
    config.persistence.save_playback_history = history;
    AppController::new(config, StateStore::open_in_memory().unwrap(), None, None)
}

/// Creates a controller that cannot start HTTP work or contact a real pod.
fn controller() -> AppController {
    let mut config = Config::for_dir("/tmp/youta-funkwhale-controller-fixture");
    config.providers.funkwhale_instance_url =
        Some(url::Url::parse("https://pod.example/").unwrap());
    let mut app = AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    app.diagnostic_only = true;
    app.show_screen(Screen::Funkwhale);
    app
}

/// Supplies one pod-scoped track and deliberately includes a transient audio URL.
fn track(pod: &str, number: u32) -> VideoSummary {
    let page = url::Url::parse(&format!("https://{pod}/library/tracks/{number}/")).unwrap();
    VideoSummary {
        video_id: page.to_string(),
        title: format!("Track {number}"),
        channel_name: "Fixture artist".to_owned(),
        channel_id: format!("https://{pod}/library/artists/9/"),
        description: "Description".to_owned(),
        duration_seconds: Some(120),
        view_count: None,
        published_at: None,
        published_text: None,
        live: false,
        was_live: false,
        orientation: VideoOrientation::Unknown,
        thumbnails: vec![Thumbnail {
            url: url::Url::parse(&format!("https://{pod}/artwork.jpg")).unwrap(),
            quality: Some("original".to_owned()),
            width: Some(1000),
            height: Some(1000),
        }],
        webpage_url: Some(page),
        stream_url: Some(url::Url::parse("https://cdn.example/audio.mp3?token=transient").unwrap()),
    }
}

/// Completes public metadata without a provider or real network request.
fn details(track: &VideoSummary) -> VideoDetails {
    VideoDetails {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        channel_name: track.channel_name.clone(),
        channel_id: track.channel_id.clone(),
        description: "Full description".to_owned(),
        duration_seconds: track.duration_seconds,
        view_count: None,
        like_count: None,
        comment_count: None,
        published_at: track.published_at,
        published_text: None,
        license: Some("CC0".to_owned()),
        rating: None,
        ratings_allowed: None,
        live: false,
        was_live: false,
        orientation: VideoOrientation::Unknown,
        keywords: vec!["experimental".to_owned()],
        thumbnails: track.thumbnails.clone(),
        webpage_url: track.webpage_url.clone(),
        stream_url: track.stream_url.clone(),
    }
}

/// Applies a fake search page through the production completion reducer.
fn complete(app: &mut AppController, tracks: Vec<VideoSummary>, next_page: Option<u32>) {
    let job = search_job(app, app.funkwhale.generation);
    app.apply_funkwhale_reply(
        job,
        Ok(Reply::Search(SearchPage {
            page: 1,
            items: tracks.into_iter().map(SearchItem::Video).collect(),
            next_page,
        })),
    );
}

/// Supplies a non-playable album identity independently from track queue metadata.
fn album(pod: &str, id: u32, title: &str) -> FunkwhaleAlbum {
    FunkwhaleAlbum {
        url: url::Url::parse(&format!("https://{pod}/library/albums/{id}")).unwrap(),
        title: title.to_owned(),
        description: "Album description".to_owned(),
        tracks_count: Some(22),
        release_date: Some("2016-10-13".to_owned()),
        thumbnails: Vec::new(),
    }
}

/// Completes an artist directory without producing playable track rows or detail lookups.
fn complete_albums(app: &mut AppController, albums: Vec<FunkwhaleAlbum>, next_page: Option<u32>) {
    let job = search_job(app, app.funkwhale.generation);
    app.apply_funkwhale_reply(
        job,
        Ok(Reply::Albums(
            FunkwhaleAlbumPage {
                page: 1,
                items: albums,
                next_page,
            },
            Some(Ok(true)),
        )),
    );
}

/// Captures exactly the selected pod and source-local request owner.
fn search_job(app: &AppController, generation: u64) -> Job {
    Job::Search {
        generation,
        pod: app.config.providers.funkwhale_instance_url.clone().unwrap(),
        query: "fixture".to_owned(),
        page: 1,
        size: app.funkwhale.page_size,
        artist: None,
        album: None,
        tag: None,
        standalone: false,
        uploader: app
            .funkwhale
            .uploader
            .as_ref()
            .map(|(actor, _)| actor.clone()),
    }
}

#[test]
fn first_use_requests_setup_without_a_default_or_http_work() {
    let mut app = controller();
    app.config.providers.funkwhale_instance_url = None;
    app.funkwhale.setup_prompted = false;
    app.show_screen(Screen::Funkwhale);
    assert!(
        app.view
            .funkwhale_setup_popup
            .as_ref()
            .unwrap()
            .url
            .is_empty()
    );
    assert!(app.config.providers.funkwhale_instance_url.is_none());
    assert!(app.funkwhale.jobs.is_empty());
    assert!(app.funkwhale.worker.is_none());
    app.dispatch(UiAction::CloseFunkwhaleSetup);
    app.show_screen(Screen::Search);
    app.show_screen(Screen::Funkwhale);
    assert!(app.view.funkwhale_setup_popup.is_none());
}

#[test]
fn setup_modal_blocks_covered_playback_actions() {
    let mut app = controller();
    app.open_funkwhale_setup();
    let before = app.config.playback.autoplay;
    app.dispatch(UiAction::ToggleAutoplay);
    assert_eq!(app.config.playback.autoplay, before);
    app.dispatch(UiAction::SetFunkwhalePodUrl(
        "pod.example/funkwhale/".to_owned(),
    ));
    assert_eq!(
        app.view.funkwhale_setup_popup.as_ref().unwrap().url,
        "pod.example/funkwhale/"
    );
}

#[test]
fn directory_selection_fills_the_draft_without_saving_or_selecting_a_default() {
    let mut app = controller();
    app.open_funkwhale_setup();
    app.open_funkwhale_pods();
    assert!(
        app.view
            .funkwhale_setup_popup
            .as_ref()
            .unwrap()
            .picker
            .as_ref()
            .unwrap()
            .loading
    );
    app.apply_funkwhale_reply(
        Job::Pods,
        Ok(Reply::Pods(vec![FunkwhalePod {
            label: "Public pod".to_owned(),
            url: url::Url::parse("https://other.example/funkwhale/").unwrap(),
            open_registrations: Some(true),
        }])),
    );
    assert_eq!(
        app.view.funkwhale_setup_popup.as_ref().unwrap().url,
        "https://pod.example/"
    );
    app.dispatch(UiAction::SelectFunkwhalePod(0));
    assert_eq!(
        app.view.funkwhale_setup_popup.as_ref().unwrap().url,
        "https://other.example/funkwhale/"
    );
    assert_eq!(
        app.config
            .providers
            .funkwhale_instance_url
            .as_ref()
            .unwrap()
            .as_str(),
        "https://pod.example/"
    );
    assert!(
        app.view
            .funkwhale_setup_popup
            .as_ref()
            .unwrap()
            .picker
            .is_none()
    );
    app.open_funkwhale_pods();
    assert!(
        !app.view
            .funkwhale_setup_popup
            .as_ref()
            .unwrap()
            .picker
            .as_ref()
            .unwrap()
            .loading
    );
}

#[test]
fn directory_failure_keeps_manual_setup_available() {
    let mut app = controller();
    app.open_funkwhale_setup();
    app.open_funkwhale_pods();
    app.apply_funkwhale_reply(Job::Pods, Err("Directory unavailable".to_owned()));
    let popup = app.view.funkwhale_setup_popup.as_ref().unwrap();
    assert_eq!(popup.url, "https://pod.example/");
    assert_eq!(
        popup.picker.as_ref().unwrap().error.as_deref(),
        Some("Directory unavailable")
    );
    assert!(!popup.picker.as_ref().unwrap().loading);
    app.dispatch(UiAction::CloseFunkwhalePodPicker);
    assert!(app.view.funkwhale_setup_popup.is_some());
}

#[test]
fn save_and_clear_pod_persist_only_explicit_changes() {
    let directory = crate::test_support::canonical_tempdir("funkwhale setup save");
    let mut app = controller();
    app.config = Config::for_dir(directory.path());
    app.open_funkwhale_setup();
    app.dispatch(UiAction::SetFunkwhalePodUrl(
        "pod.example/funkwhale".to_owned(),
    ));
    app.dispatch(UiAction::SaveFunkwhaleSetup);
    assert!(app.view.funkwhale_setup_popup.is_none());
    assert_eq!(
        app.config
            .providers
            .funkwhale_instance_url
            .as_ref()
            .unwrap()
            .as_str(),
        "https://pod.example/funkwhale/"
    );
    app.open_funkwhale_setup();
    app.dispatch(UiAction::ClearFunkwhalePodUrl);
    app.dispatch(UiAction::SaveFunkwhaleSetup);
    assert!(app.config.providers.funkwhale_instance_url.is_none());
    assert!(app.view.funkwhale_setup_popup.is_none());
    assert!(
        !std::fs::read_to_string(app.config.config_file())
            .unwrap()
            .contains("funkwhale_instance_url")
    );
}

#[test]
fn rejected_pod_drafts_leave_old_pod_and_results_unchanged() {
    let mut app = controller();
    complete(&mut app, vec![track("pod.example", 1)], None);
    for draft in [
        "https://user:password@pod.example",
        "file:///tmp/pod",
        "https://pod.example/?token=secret",
        "https://pod.example/#fragment",
    ] {
        app.open_funkwhale_setup();
        app.dispatch(UiAction::SetFunkwhalePodUrl(draft.to_owned()));
        app.dispatch(UiAction::SaveFunkwhaleSetup);
        assert!(
            app.view
                .funkwhale_setup_popup
                .as_ref()
                .unwrap()
                .error
                .is_some()
        );
        assert_eq!(
            app.config
                .providers
                .funkwhale_instance_url
                .as_ref()
                .unwrap()
                .as_str(),
            "https://pod.example/"
        );
        assert_eq!(app.funkwhale.items.len(), 1);
    }
}

#[test]
fn a_new_query_freezes_one_viewport_stride() {
    let mut app = controller();
    app.update_funkwhale_search_page_capacity(10);
    app.submit_funkwhale_search("small".to_owned());
    assert_eq!(app.funkwhale.page_size, 10);
    app.update_funkwhale_search_page_capacity(100);
    assert_eq!(app.funkwhale.page_size, 10);
    app.submit_funkwhale_search("large".to_owned());
    assert_eq!(app.funkwhale.page_size, 100);
    app.update_funkwhale_search_page_capacity(1000);
    assert_eq!(
        app.funkwhale.capacity,
        Some(crate::view::SearchPageCapacities::fixed(100))
    );
}

#[test]
fn source_owned_funkwhale_query_chooses_header_capacity_only_for_its_first_page() {
    let mut app = controller();
    app.update_funkwhale_search_page_capacities(crate::view::SearchPageCapacities {
        with_heading: 9,
        without_heading: 10,
    });
    app.view.search_query = "another tab's query".to_owned();
    app.submit_funkwhale_search(String::new());
    assert_eq!(app.funkwhale.page_size, 20);
    app.submit_funkwhale_search("own query".to_owned());
    assert_eq!(app.funkwhale.page_size, 9);
    app.update_funkwhale_search_page_capacities(crate::view::SearchPageCapacities {
        with_heading: 90,
        without_heading: 91,
    });
    assert_eq!(app.funkwhale.page_size, 9);
    complete(&mut app, vec![track("pod.example", 1)], None);
    app.open_funkwhale_artist();
    assert_eq!(
        app.funkwhale.page_size, 91,
        "an empty artist heading uses its reclaimed row"
    );
}

#[test]
fn search_rows_are_compact_and_only_advertise_a_real_continuation() {
    let mut app = controller();
    complete(
        &mut app,
        vec![track("pod.example", 1), track("pod.example", 2)],
        Some(2),
    );
    assert_eq!(app.view.rows.len(), 3);
    assert!(app.view.rows.iter().all(|row| row.compact));
    assert_eq!(app.view.rows[2].title, "Load more tracks...");
    complete(&mut app, vec![track("pod.example", 3)], None);
    assert_eq!(app.view.rows.len(), 3);
    assert!(app.view.rows.iter().all(|row| row.media_id.is_some()));
}

#[test]
fn pagination_deduplicates_and_bounds_retained_rows() {
    let mut app = controller();
    complete(
        &mut app,
        vec![track("pod.example", 1), track("pod.example", 1)],
        Some(2),
    );
    assert_eq!(app.funkwhale.items.len(), 1);
    complete(
        &mut app,
        (2..=1005).map(|id| track("pod.example", id)).collect(),
        Some(3),
    );
    assert_eq!(app.funkwhale.items.len(), 1000);
    assert!(app.funkwhale.next_page.is_none());
}

#[test]
fn continuation_scrolls_only_the_new_page_and_can_be_abandoned() {
    let mut app = controller();
    complete(
        &mut app,
        vec![track("pod.example", 1), track("pod.example", 2)],
        Some(2),
    );
    app.view.selected = 2;
    app.update_funkwhale_detail();
    app.activate_funkwhale_selection();
    complete(
        &mut app,
        vec![track("pod.example", 3), track("pod.example", 4)],
        Some(3),
    );
    assert_eq!(app.view.selected, 4);
    app.activate_funkwhale_selection();
    app.view.selected = 0;
    app.update_funkwhale_detail();
    complete(&mut app, vec![track("pod.example", 5)], None);
    assert_eq!(app.view.selected, 0);
}

#[test]
fn final_page_selects_last_track_instead_of_a_nonexistent_continuation() {
    let mut app = controller();
    complete(&mut app, vec![track("pod.example", 1)], Some(2));
    app.view.selected = 1;
    app.update_funkwhale_detail();
    app.activate_funkwhale_selection();
    complete(&mut app, vec![track("pod.example", 2)], None);
    assert_eq!(app.view.selected, 1);
    assert_eq!(
        app.view.rows[1].media_id,
        Some(MediaId::new(
            SourceKind::Funkwhale,
            track("pod.example", 2).video_id
        ))
    );
}

#[test]
fn obsolete_searches_and_details_cannot_replace_newer_owners() {
    let mut app = controller();
    let old = search_job(&app, app.funkwhale.generation);
    app.submit_funkwhale_search("new".to_owned());
    app.apply_funkwhale_reply(
        old,
        Ok(Reply::Search(SearchPage {
            page: 1,
            items: vec![SearchItem::Video(track("pod.example", 1))],
            next_page: None,
        })),
    );
    assert!(app.funkwhale.items.is_empty());
    complete(
        &mut app,
        vec![track("pod.example", 2), track("pod.example", 3)],
        None,
    );
    app.view.selected = 1;
    app.update_funkwhale_detail();
    app.apply_funkwhale_reply(
        Job::Details {
            generation: app.funkwhale.generation,
            id: track("pod.example", 2).video_id,
        },
        Ok(Reply::DetailsWithUpload(
            details(&track("pod.example", 2)),
            None,
        )),
    );
    assert_eq!(app.view.details.as_ref().unwrap().title, "Track 3");
}

#[test]
fn background_completion_retains_its_tab_without_repainting_another() {
    let mut app = controller();
    app.show_screen(Screen::Search);
    let rows = app.view.rows.clone();
    let detail = app.view.details.clone();
    complete(&mut app, vec![track("pod.example", 1)], None);
    assert_eq!(app.view.rows, rows);
    assert_eq!(app.view.details, detail);
    app.show_screen(Screen::Funkwhale);
    assert_eq!(app.view.rows[0].title, "Track 1");
}

#[test]
fn metadata_links_artwork_and_license_do_not_invent_youtube_counts() {
    let summary = track("pod.example", 1);
    let full = details(&summary);
    let view = detail_view(&summary, Some(&full));
    assert_eq!(view.webpage_url, summary.webpage_url);
    assert_eq!(
        view.channel_webpage_url.as_ref().unwrap().as_str(),
        summary.channel_id
    );
    assert_eq!(
        view.expanded_thumbnail_url,
        Some(summary.thumbnails[0].url.clone())
    );
    assert_eq!(view.license, "CC0");
    assert!(view.description.contains("experimental"));
    assert!(view.views.is_empty());
    assert!(view.likes.is_empty());
}

#[test]
fn funkwhale_license_codes_have_readable_labels_and_one_canonical_link() {
    let summary = track("pod.example", 1);
    let mut full = details(&summary);
    full.license = Some("cc-by-4.0".to_owned());
    let view = detail_view(&summary, Some(&full));
    assert_eq!(view.license, "CC BY 4.0");
    let links = view
        .links
        .iter()
        .filter(|link| link.prefix == "License: ")
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].label, "CC BY 4.0");
    assert_eq!(links[0].url, "https://creativecommons.org/licenses/by/4.0/");
    assert_eq!(links[0].presentation, DetailLinkPresentation::LabelAndUrl);
    assert!(links[0].internal_target.is_none());
    assert_eq!(
        full.license.as_deref(),
        Some("cc-by-4.0"),
        "display does not change provider rights metadata"
    );
}

#[test]
fn funkwhale_unknown_or_missing_licenses_remain_honest_and_unlinked() {
    let summary = track("pod.example", 1);
    let mut full = details(&summary);
    for raw in [None, Some("Custom noncommercial terms"), Some("cc-by-5.0")] {
        full.license = raw.map(str::to_owned);
        let view = detail_view(&summary, Some(&full));
        assert_eq!(view.license, raw.unwrap_or_default());
        assert!(!view.links.iter().any(|link| link.prefix == "License: "));
    }
}

#[test]
fn selected_funkwhale_metadata_projects_real_json_listenings_without_extra_count_requests() {
    let pod = RecentPodFixture::new(false);
    let mut app = recent_controller(&pod, false);
    app.show_screen(Screen::Funkwhale);
    finish_recent_requests(&mut app, |app| app.funkwhale.items.len() == 20);
    let details = app.view.details.as_ref().unwrap();
    assert_eq!(details.license, "CC BY 4.0");
    assert!(details.description.contains("Listenings: 7"));
    assert!(
        !details.description.contains("999"),
        "downloads are not listening totals"
    );
    assert!(details.views.is_empty());
    assert_eq!(
        pod.requests().len(),
        2,
        "only catalogue and selected details, no global history scan"
    );
}

#[test]
fn queue_identity_is_pod_scoped_and_never_contains_the_resolved_stream() {
    let first = queue_item(&track("pod.example", 1)).unwrap();
    let second = queue_item(&track("other.example", 1)).unwrap();
    assert_ne!(first.media.id, second.media.id);
    assert_eq!(first.playback_location, first.media.webpage_url.as_str());
    assert!(!serde_json::to_string(&first).unwrap().contains("transient"));
    let mut bad = track("pod.example", 1);
    bad.video_id = "1".to_owned();
    assert!(queue_item(&bad).is_err());
}

#[test]
fn stale_or_mismatched_playback_completion_does_not_replace_current_media() {
    let mut app = controller();
    let summary = track("pod.example", 1);
    let item = queue_item(&summary).unwrap();
    let generation = app.funkwhale.playback_generation;
    app.cancel_funkwhale_playback();
    app.apply_funkwhale_reply(
        Job::Playback {
            generation,
            item: Box::new(item.clone()),
            queued: false,
            origin: None,
        },
        Ok(Reply::Playback(
            details(&summary),
            summary.stream_url.clone().unwrap(),
        )),
    );
    assert!(app.current_media.is_none());
    app.apply_funkwhale_reply(
        Job::Playback {
            generation: app.funkwhale.playback_generation,
            item: Box::new(item),
            queued: false,
            origin: None,
        },
        Ok(Reply::Playback(
            details(&track("other.example", 1)),
            summary.stream_url.unwrap(),
        )),
    );
    assert!(app.current_media.is_none());
    assert!(app.view.status_line.contains("different track"));
}

#[test]
fn playback_failures_identify_the_exact_pod_track() {
    let mut app = controller();
    let item = queue_item(&track("pod.example", 1)).unwrap();
    app.apply_funkwhale_reply(
        Job::Playback {
            generation: app.funkwhale.playback_generation,
            item: Box::new(item),
            queued: false,
            origin: None,
        },
        Err("provider returned HTTP status 403".to_owned()),
    );
    assert!(
        app.view
            .status_line
            .contains("https://pod.example/library/tracks/1/")
    );
    assert!(app.view.status_line.contains("403"));
    let popup = app
        .view
        .error_popup
        .as_ref()
        .expect("failure must be visible in TTY, not only an unrendered status line");
    assert!(
        popup
            .report
            .contains("https://pod.example/library/tracks/1/")
    );
    assert!(popup.report.contains("403"));
}

#[test]
fn recent_catalogue_fetches_one_page_without_geometry_or_saved_history() {
    for history in [true, false] {
        let pod = RecentPodFixture::new(false);
        let mut app = recent_controller(&pod, history);
        assert!(!app.diagnostic_only);
        assert!(app.funkwhale.capacity.is_none());
        app.show_screen(Screen::Funkwhale);
        finish_recent_requests(&mut app, |app| app.funkwhale.items.len() == 20);
        assert_eq!(app.funkwhale.page_size, 20);
        assert_eq!(app.funkwhale.next_page, Some(2));
        assert_eq!(app.view.rows.len(), 21);
        assert_eq!(app.view.rows[0].title, "Recent track 1");
        assert_eq!(app.view.rows[20].title, "Load more tracks...");
        assert!(app.funkwhale.query.is_empty());
        for _ in 0..3 {
            app.poll_funkwhale_worker();
        }
        let requests = pod.requests();
        let catalogue = requests
            .iter()
            .filter(|target| target.split('?').next() == Some("/api/v1/tracks/"))
            .collect::<Vec<_>>();
        assert_eq!(catalogue.len(), 1, "one catalogue request: {requests:?}");
        let target = pod.base.join(catalogue[0]).unwrap();
        let query = target.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            query.get("page_size").map(|value| value.as_ref()),
            Some("20")
        );
        assert_eq!(query.get("page").map(|value| value.as_ref()), Some("1"));
        assert_eq!(
            query.get("ordering").map(|value| value.as_ref()),
            Some("-creation_date")
        );
        assert_eq!(
            requests
                .iter()
                .filter(|target| target.as_str() == "/api/v1/tracks/1/")
                .count(),
            1
        );
        assert_eq!(
            requests.len(),
            2,
            "only catalogue and lazy selected metadata"
        );
    }
}

#[test]
fn recent_catalogue_after_saving_a_pod_does_not_need_geometry_or_history() {
    let pod = RecentPodFixture::new(false);
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::for_dir(directory.path());
    config.persistence.save_playback_history = false;
    let mut app = AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    app.show_screen(Screen::Funkwhale);
    app.dispatch(UiAction::SetFunkwhalePodUrl(pod.base.to_string()));
    app.dispatch(UiAction::SaveFunkwhaleSetup);
    assert!(app.view.funkwhale_setup_popup.is_none());
    finish_recent_requests(&mut app, |app| app.funkwhale.items.len() == 20);
    assert_eq!(pod.requests().len(), 2);
}

#[test]
fn recent_catalogue_never_fetches_hidden_unconfigured_or_edited_tabs() {
    let pod = RecentPodFixture::new(false);
    let mut app = recent_controller(&pod, false);
    app.show_screen(Screen::Local);
    app.poll_funkwhale_worker();
    assert!(!app.funkwhale.recent_attempted);
    assert!(app.funkwhale.worker.is_none());
    assert!(pod.requests().is_empty());

    app.show_screen(Screen::Funkwhale);
    app.view.search_editing = true;
    app.view.search_query = "unfinished edit".to_owned();
    app.poll_funkwhale_worker();
    assert!(!app.funkwhale.recent_attempted);
    assert_eq!(app.view.search_query, "unfinished edit");
    assert!(app.funkwhale.worker.is_none());
    assert!(pod.requests().is_empty());

    app.view.search_editing = false;
    app.config.providers.funkwhale_instance_url = None;
    app.show_screen(Screen::Funkwhale);
    app.poll_funkwhale_worker();
    assert!(app.view.funkwhale_setup_popup.is_some());
    assert!(app.funkwhale.worker.is_none());
    assert!(pod.requests().is_empty());
}

#[test]
fn recent_catalogue_accepts_a_whitespace_only_inactive_query() {
    let pod = RecentPodFixture::new(false);
    let mut app = recent_controller(&pod, true);
    app.show_screen(Screen::Funkwhale);
    app.funkwhale.query = " \t ".to_owned();
    app.view.search_query.clone_from(&app.funkwhale.query);
    finish_recent_requests(&mut app, |app| app.funkwhale.items.len() == 20);
    assert!(app.view.search_query.is_empty());
    assert_eq!(pod.requests().len(), 2);
}

#[test]
fn recent_catalogue_failure_is_visible_without_diagnostics_or_automatic_retries() {
    let pod = RecentPodFixture::new(true);
    let mut app = recent_controller(&pod, true);
    app.update_funkwhale_search_page_capacity(20);
    app.show_screen(Screen::Funkwhale);
    finish_recent_requests(&mut app, |app| app.view.error_popup.is_some());
    let popup = app.view.error_popup.as_ref().unwrap();
    assert!(popup.report.contains(pod.base.as_str()));
    assert!(popup.report.contains("503"));
    assert!(!popup.report.contains("OS:"));
    assert!(!popup.reportable);
    assert!(app.view.search_activity.is_none());
    app.view.error_popup = None;
    app.show_screen(Screen::Local);
    app.show_screen(Screen::Funkwhale);
    for _ in 0..3 {
        app.poll_funkwhale_worker();
    }
    assert_eq!(pod.requests().len(), 1);
    assert!(app.funkwhale.worker.is_none());
    assert!(app.view.error_popup.is_none());
}

#[test]
fn recent_catalogue_is_deferred_until_visible_and_keeps_history_disabled_queries_empty() {
    let mut app = controller();
    app.view.screen = Screen::Search;
    app.poll_funkwhale_worker();
    assert!(!app.funkwhale.recent_attempted);
    app.view.screen = Screen::Funkwhale;
    app.poll_funkwhale_worker();
    assert!(app.funkwhale.recent_attempted);
    assert_eq!(app.funkwhale.page_size, 20);
    assert_eq!(app.view.search_activity, Some(SearchActivity::Funkwhale));
    app.finish_search_activity(SearchActivity::Funkwhale);
    app.poll_funkwhale_worker();
    assert!(app.view.search_activity.is_none());

    let mut app = controller();
    app.config.persistence.save_playback_history = false;
    app.poll_funkwhale_worker();
    assert!(app.funkwhale.recent_attempted);
    assert!(app.view.rows.is_empty());
    assert!(app.funkwhale.query.is_empty());
    assert_eq!(app.view.search_activity, Some(SearchActivity::Funkwhale));
}

#[test]
fn disabling_history_revokes_dormant_restoration_without_erasing_explicit_private_results() {
    let mut app = controller();
    app.funkwhale.query = "old saved query".to_owned();
    app.funkwhale.restore = true;
    app.funkwhale.restore_selected = Some(3);
    app.view.screen = Screen::Search;
    app.config.persistence.save_playback_history = false;
    app.poll_funkwhale_worker();
    assert!(!app.funkwhale.restore);
    assert!(app.funkwhale.restore_selected.is_none());
    app.config.persistence.save_playback_history = true;
    app.view.screen = Screen::Funkwhale;
    app.poll_funkwhale_worker();
    assert!(app.view.search_activity.is_none());
    app.config.persistence.save_playback_history = false;
    app.submit_funkwhale_search("explicit private query".to_owned());
    complete(&mut app, vec![track("pod.example", 8)], None);
    app.poll_funkwhale_worker();
    assert_eq!(app.funkwhale.items.len(), 1);
    assert_eq!(app.funkwhale.query, "explicit private query");
}

#[test]
fn artist_album_directory_never_turns_track_results_into_playable_album_rows() {
    let mut app = controller();
    complete(&mut app, vec![track("pod.example", 1)], None);
    app.open_funkwhale_artist();
    complete(&mut app, vec![track("pod.example", 2)], None);
    assert!(app.view.rows.iter().all(|row| row.media_id.is_none()));
    assert!(app.selected_funkwhale_queue_item().is_err());
    assert!(app.funkwhale.items.is_empty());
}

/// Enters artist browsing from a real selected original-pod track without fetching seed metadata.
fn open_fixture_artist(app: &mut AppController, pod: &RecentPodFixture, artist: u32) {
    app.diagnostic_only = true;
    app.show_screen(Screen::Funkwhale);
    let mut summary = track("pod.example", 1);
    let page = pod.base.join("library/tracks/1").unwrap();
    summary.video_id = page.to_string();
    summary.webpage_url = Some(page);
    summary.channel_id = pod
        .base
        .join(&format!("library/artists/{artist}"))
        .unwrap()
        .to_string();
    summary.channel_name = format!("Artist {artist}");
    app.submit_funkwhale_search("prior catalogue".to_owned());
    complete(app, vec![summary], None);
    app.diagnostic_only = false;
    app.open_funkwhale_artist();
}

#[test]
fn artist_albums_are_nonplayable_paginated_folders_and_enter_opens_album_track_order() {
    let pod = RecentPodFixture::artist();
    let mut app = recent_controller(&pod, true);
    open_fixture_artist(&mut app, &pod, 5796);
    finish_recent_requests(&mut app, |app| app.funkwhale.albums.len() == 1);
    assert_eq!(app.view.rows[0].title, "ADL");
    assert_eq!(app.view.rows[1].title, "Load more albums...");
    assert_eq!(
        app.view.rows.len(),
        2,
        "an empty Standalone folder is hidden"
    );
    assert!(app.view.rows.iter().all(|row| row.media_id.is_none()));
    assert!(app.funkwhale.items.is_empty());
    assert!(app.selected_funkwhale_queue_item().is_err());
    assert!(app.current_media.is_none());
    assert_eq!(
        pod.requests().len(),
        3,
        "v1 fallback, v2 album page, and one bounded standalone probe"
    );

    app.view.selected = 1;
    app.update_funkwhale_detail();
    app.activate_funkwhale_selection();
    finish_recent_requests(&mut app, |app| app.funkwhale.albums.len() == 2);
    assert_eq!(app.view.rows[1].title, "Second album");
    assert_eq!(app.view.rows.len(), 2);
    assert!(app.funkwhale.next_page.is_none());
    assert_eq!(
        pod.requests().len(),
        4,
        "page two needs explicit activation and does not repeat the standalone probe"
    );

    app.view.selected = 0;
    app.update_funkwhale_detail();
    app.activate_funkwhale_selection();
    finish_recent_requests(&mut app, |app| app.funkwhale.items.len() == 3);
    assert!(app.funkwhale.albums.is_empty());
    assert!(
        app.funkwhale
            .album
            .as_ref()
            .unwrap()
            .0
            .ends_with("/library/albums/4408")
    );
    assert_eq!(app.view.rows[0].title, "Recent track 27108");
    assert_eq!(app.view.rows[1].title, "Recent track 27109");
    assert_eq!(app.view.rows[2].title, "Recent track 27110");
    let requests = pod.requests();
    let album_request = requests
        .iter()
        .find(|target| target.starts_with("/api/v2/tracks/?") && target.contains("album=4408"))
        .unwrap();
    let url = pod.base.join(album_request).unwrap();
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "album" && value == "4408")
    );
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "ordering" && value == "disc_number,position")
    );
    assert!(
        app.current_media.is_none(),
        "opening an album must not start playback"
    );
    assert!(app.back_funkwhale_artist());
    assert_eq!(app.view.rows[0].title, "ADL");
    assert_eq!(app.view.rows[1].title, "Second album");
    assert!(app.view.rows.iter().all(|row| row.media_id.is_none()));
    app.diagnostic_only = true;
    assert!(app.back_funkwhale_artist());
    assert_eq!(app.view.search_query, "prior catalogue");
    assert_eq!(app.view.rows[0].title, "Track 1");
}

#[test]
fn empty_artist_has_neither_album_nor_standalone_folders() {
    let pod = RecentPodFixture::artist();
    let mut app = recent_controller(&pod, true);
    open_fixture_artist(&mut app, &pod, 5795);
    finish_recent_requests(&mut app, |app| app.view.search_activity.is_none());
    assert!(app.view.rows.is_empty(), "no empty Standalone folder");
    assert!(app.funkwhale.next_page.is_none());
    app.activate_funkwhale_selection();
    assert_eq!(
        pod.requests().len(),
        3,
        "empty rows never start another request"
    );
}

#[test]
fn mixed_artist_probes_nulls_first_once_and_keeps_standalone_across_album_pagination() {
    let pod = RecentPodFixture::artist();
    let mut app = recent_controller(&pod, true);
    open_fixture_artist(&mut app, &pod, 5797);
    app.config.providers.funkwhale_instance_url =
        Some(url::Url::parse("https://different-pod.invalid/").unwrap());
    finish_recent_requests(&mut app, |app| app.view.search_activity.is_none());
    assert_eq!(app.view.rows[1].title, "Standalone tracks");
    assert_eq!(app.view.rows[2].title, "Load more albums...");
    let probes = pod
        .requests()
        .into_iter()
        .filter(|target| target.starts_with("/api/v2/tracks/?"))
        .collect::<Vec<_>>();
    assert_eq!(
        probes.len(),
        1,
        "find later-page standalone tracks with one bounded probe"
    );
    let url = pod.base.join(&probes[0]).unwrap();
    let query = url.query_pairs().collect::<HashMap<_, _>>();
    for (key, value) in [
        ("artist", "5797"),
        ("page", "1"),
        ("page_size", "1"),
        ("ordering", "-album__title"),
        ("playable", "true"),
        ("include_channels", "true"),
    ] {
        assert_eq!(query.get(key).map(|value| value.as_ref()), Some(value));
    }
    assert!(!query.contains_key("album__isnull"));
    assert!(!query.contains_key("has_album"));
    app.view.selected = 2;
    app.update_funkwhale_detail();
    app.activate_funkwhale_selection();
    finish_recent_requests(&mut app, |app| app.funkwhale.albums.len() == 2);
    assert_eq!(app.view.rows[2].title, "Standalone tracks");
    assert_eq!(pod.requests().len(), 4);
    app.view.selected = 0;
    app.update_funkwhale_detail();
    app.activate_funkwhale_selection();
    finish_recent_requests(&mut app, |app| app.funkwhale.items.len() == 3);
    assert!(app.back_funkwhale_artist());
    assert_eq!(app.view.rows[2].title, "Standalone tracks");
    assert_eq!(app.funkwhale.standalone_presence, Some(true));
    assert_eq!(
        pod.requests()
            .iter()
            .filter(|target| target.contains("ordering=-album__title"))
            .count(),
        1,
        "Back and pagination reuse the original-pod result"
    );
    app.view.selected = 2;
    app.update_funkwhale_detail();
    app.activate_funkwhale_selection();
    finish_recent_requests(&mut app, |app| app.view.search_activity.is_none());
    assert!(
        app.funkwhale.items.is_empty(),
        "ordinary first-page tracks are album-bound"
    );
    assert_eq!(app.view.rows[0].title, "Load more tracks...");
    app.activate_funkwhale_selection();
    finish_recent_requests(&mut app, |app| app.funkwhale.items.len() == 1);
    assert_eq!(app.view.rows[0].title, "Recent track 27085");
    assert!(app.back_funkwhale_artist());
    assert_eq!(app.view.rows[2].title, "Standalone tracks");
}

#[test]
fn failed_standalone_probe_preserves_album_results_and_accessible_unknown_folder() {
    let pod = RecentPodFixture::artist();
    let mut app = recent_controller(&pod, true);
    open_fixture_artist(&mut app, &pod, 5798);
    finish_recent_requests(&mut app, |app| app.view.search_activity.is_none());
    assert_eq!(app.view.rows[0].title, "ADL");
    assert_eq!(app.view.rows[1].title, "Standalone tracks");
    assert_eq!(app.view.rows[2].title, "Load more albums...");
    assert_eq!(app.funkwhale.standalone_presence, None);
    assert_eq!(
        pod.requests().len(),
        3,
        "a probe error does not trigger catalogue scans"
    );
}

#[test]
fn artist_without_albums_opens_only_explicit_standalone_tracks_without_scanning_more_pages() {
    let pod = RecentPodFixture::artist();
    let mut app = recent_controller(&pod, false);
    open_fixture_artist(&mut app, &pod, 5794);
    finish_recent_requests(&mut app, |app| app.view.search_activity.is_none());
    assert!(app.funkwhale.albums.is_empty());
    assert_eq!(app.view.rows.len(), 1);
    assert_eq!(app.view.rows[0].title, "Standalone tracks");
    assert!(app.selected_funkwhale_queue_item().is_err());
    assert_eq!(pod.requests().len(), 3);
    app.activate_funkwhale_selection();
    finish_recent_requests(&mut app, |app| app.funkwhale.items.len() == 2);
    assert!(app.funkwhale.standalone);
    assert_eq!(app.view.rows[0].title, "Recent track 27085");
    assert_eq!(app.view.rows[1].title, "Recent track 27084");
    assert_eq!(app.view.rows[2].title, "Load more tracks...");
    assert!(
        app.funkwhale
            .items
            .iter()
            .all(|track| !track.video_id.ends_with("/27083"))
    );
    assert!(app.selected_funkwhale_queue_item().is_ok());
    let requests = pod.requests();
    let standalone = requests
        .iter()
        .filter(|target| target.starts_with("/api/v2/tracks/?"))
        .filter(|target| !target.contains("ordering=-album__title"))
        .collect::<Vec<_>>();
    assert_eq!(
        standalone.len(),
        1,
        "do not scan later artist pages automatically"
    );
    let url = pod.base.join(standalone[0]).unwrap();
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "artist" && value == "5794")
    );
    assert!(
        !url.query_pairs()
            .any(|(key, _)| key == "album__isnull" || key == "has_album")
    );
    assert!(app.back_funkwhale_artist());
    assert_eq!(app.view.rows[0].title, "Standalone tracks");
    assert!(app.funkwhale.items.is_empty());
    app.diagnostic_only = true;
    assert!(app.back_funkwhale_artist());
    assert_eq!(app.view.search_query, "prior catalogue");
}

#[test]
fn artist_navigation_uses_selected_identity_and_back_restores_the_exact_catalogue() {
    let mut app = controller();
    app.submit_funkwhale_search("original query".to_owned());
    complete(
        &mut app,
        vec![track("pod.example", 1), track("pod.example", 2)],
        Some(2),
    );
    app.view.selected = 1;
    app.update_funkwhale_detail();
    assert!(app.can_open_funkwhale_artist());
    app.open_funkwhale_artist();
    assert_eq!(app.view.screen, Screen::Funkwhale);
    assert_eq!(
        app.funkwhale.artist.as_ref().unwrap().0,
        "https://pod.example/library/artists/9/"
    );
    assert!(app.funkwhale.items.is_empty());
    complete_albums(&mut app, vec![album("pod.example", 4408, "ADL")], None);
    assert_eq!(app.view.rows[0].title, "ADL");
    assert_eq!(app.view.rows[1].title, "Standalone tracks");
    assert!(app.view.rows.iter().all(|row| row.media_id.is_none()));
    assert!(app.back_funkwhale_artist());
    assert_eq!(app.view.rows[1].title, "Track 2");
    assert_eq!(app.view.selected, 1);
    assert_eq!(app.view.search_query, "original query");
    assert_eq!(app.funkwhale.next_page, Some(2));
    assert!(app.funkwhale.artist.is_none());
    assert!(!app.back_funkwhale_artist());
    app.funkwhale.items[1].channel_id = "https://another.example/library/artists/9".to_owned();
    assert!(!app.can_open_funkwhale_artist());
    app.view.selected = app.funkwhale.items.len();
    assert!(!app.can_open_funkwhale_artist());
}

#[test]
fn details_include_both_track_and_artist_links() {
    let summary = track("pod.example", 27084);
    let view = detail_view(&summary, None);
    assert!(
        view.links
            .iter()
            .any(|link| link.label == "Track" && link.url == summary.video_id)
    );
    assert!(
        view.links
            .iter()
            .any(|link| link.label == "Artist" && link.url == summary.channel_id)
    );
}

#[test]
fn album_link_is_unified_internal_and_navigates_back_on_its_own_pod() {
    let mut app = controller();
    app.submit_funkwhale_search("original".to_owned());
    let mut summary = track("pod.example", 1);
    summary.description =
        "Album: Named album\nSize: 12 MiB\nAlbum page: https://pod.example/library/albums/22"
            .to_owned();
    complete(&mut app, vec![summary.clone()], Some(2));
    let view = app.view.details.as_ref().unwrap();
    assert!(!view.description.contains("Album:"));
    assert!(!view.description.contains("Album page:"));
    assert!(view.description.contains("Size: 12 MiB"));
    let link = view
        .links
        .iter()
        .find(|link| {
            matches!(
                link.internal_target,
                Some(DetailLinkInternalTarget::FunkwhaleAlbum(_))
            )
        })
        .unwrap();
    assert_eq!(link.label, "Named album");
    assert_eq!(link.url, "https://pod.example/library/albums/22");
    assert!(view.links.iter().any(|link| matches!(
        link.internal_target,
        Some(DetailLinkInternalTarget::FunkwhaleArtist(_))
    )));
    app.open_funkwhale_album(link.url.clone());
    assert_eq!(
        app.funkwhale.album.as_ref().unwrap().0,
        "https://pod.example/library/albums/22"
    );
    assert!(app.funkwhale.artist.is_none());
    app.config.providers.funkwhale_instance_url =
        Some(url::Url::parse("https://other.example/").unwrap());
    assert_eq!(
        app.funkwhale_catalogue_pod().unwrap().as_str(),
        "https://pod.example/"
    );
    complete(&mut app, vec![track("pod.example", 7)], None);
    assert!(app.back_funkwhale_artist());
    assert!(app.funkwhale.album.is_none());
    assert_eq!(app.view.search_query, "original");
    assert_eq!(app.funkwhale.next_page, Some(2));
    assert_eq!(app.view.rows[0].title, "Track 1");
}

#[test]
fn album_action_rejects_stale_unadvertised_and_cross_pod_pages() {
    let mut app = controller();
    let mut summary = track("pod.example", 1);
    summary.description =
        "Album: Named album\nAlbum page: https://pod.example/library/albums/22".to_owned();
    complete(&mut app, vec![summary], None);
    for album in [
        "https://pod.example/library/albums/23",
        "https://other.example/library/albums/22",
        "https://pod.example/library/albums/022",
    ] {
        app.open_funkwhale_album(album.to_owned());
        assert!(app.funkwhale.album.is_none());
    }
    app.view.screen = Screen::Search;
    app.open_funkwhale_album("https://pod.example/library/albums/22".to_owned());
    assert!(app.funkwhale.album.is_none());
}

#[test]
fn album_metadata_projection_preserves_similarly_named_user_description_lines() {
    let mut summary = track("pod.example", 1);
    summary.description = "Album: Named album\nAlbum: user commentary\nAlbum page: https://pod.example/library/albums/999\nAlbum page: https://pod.example/library/albums/22".to_owned();
    let view = detail_view(&summary, None);
    let link = view
        .links
        .iter()
        .find(|link| {
            matches!(
                link.internal_target,
                Some(DetailLinkInternalTarget::FunkwhaleAlbum(_))
            )
        })
        .unwrap();
    assert_eq!(link.url, "https://pod.example/library/albums/22");
    assert_eq!(
        view.description,
        "Album: user commentary\nAlbum page: https://pod.example/library/albums/999"
    );
}

#[test]
fn default_recent_catalogue_keeps_twenty_track_stride_without_automatic_next_pages() {
    let mut app = controller();
    app.update_funkwhale_search_page_capacity(100);
    app.poll_funkwhale_worker();
    assert_eq!(app.funkwhale.page_size, 20);
    complete(
        &mut app,
        (1..=20)
            .map(|number| track("pod.example", number))
            .collect(),
        Some(2),
    );
    app.poll_funkwhale_worker();
    assert_eq!(app.funkwhale.items.len(), 20);
    assert_eq!(app.funkwhale.next_page, Some(2));
    assert!(app.view.search_activity.is_none());
    app.update_funkwhale_search_page_capacity(5);
    app.view.selected = 20;
    app.activate_funkwhale_selection();
    assert_eq!(app.funkwhale.page_size, 20);
}

#[test]
fn artist_rows_show_only_albums_and_pagination_retains_the_original_pod() {
    let mut app = controller();
    complete(&mut app, vec![track("pod.example", 1)], None);
    app.open_funkwhale_artist();
    complete_albums(
        &mut app,
        vec![album("pod.example", 2, "Fixture album")],
        Some(2),
    );
    assert_eq!(app.view.rows[0].subtitle, "22 tracks · 2016-10-13");
    assert_eq!(app.view.rows[1].title, "Standalone tracks");
    assert_eq!(app.view.rows[2].title, "Load more albums...");
    assert!(app.view.rows.iter().all(|row| row.media_id.is_none()));
    assert!(app.selected_funkwhale_queue_item().is_err());
    assert!(
        app.view
            .details
            .as_ref()
            .unwrap()
            .links
            .iter()
            .any(|link| link.url.ends_with("/library/albums/2"))
    );
    app.config.providers.funkwhale_instance_url =
        Some(url::Url::parse("https://other.example/").unwrap());
    assert_eq!(
        app.funkwhale_catalogue_pod().unwrap().as_str(),
        "https://pod.example/"
    );
}

#[test]
fn tags_are_inline_exact_unicode_links_and_back_restores_the_previous_artist_catalogue() {
    let mut app = controller();
    complete(&mut app, vec![track("pod.example", 1)], None);
    app.open_funkwhale_artist();
    complete_albums(&mut app, Vec::new(), None);
    app.activate_funkwhale_selection();
    assert!(app.funkwhale.standalone);
    let summary = track("pod.example", 2);
    complete(&mut app, vec![summary.clone()], None);
    let tag = "Ambient & музыка / #field recording";
    let mut full = details(&summary);
    full.keywords = vec![tag.to_owned()];
    app.apply_funkwhale_reply(
        Job::Details {
            generation: app.funkwhale.generation,
            id: summary.video_id.clone(),
        },
        Ok(Reply::DetailsWithUpload(full, None)),
    );
    let view = app.view.details.as_ref().unwrap();
    let link = view.links.iter().find(|link| matches!(&link.internal_target, Some(DetailLinkInternalTarget::FunkwhaleTag(value)) if value == tag)).unwrap();
    let range = link.description_range.as_ref().unwrap();
    assert_eq!(&view.description[range.start_byte..range.end_byte], tag);
    assert_eq!(link.presentation, DetailLinkPresentation::LabelOnly);
    app.search_funkwhale_tag(tag.to_owned());
    assert_eq!(app.funkwhale.tag.as_ref().unwrap().0, tag);
    assert!(app.funkwhale.artist.is_none());
    assert_eq!(app.view.search_query, tag);
    complete(&mut app, vec![track("pod.example", 3)], None);
    assert!(app.back_funkwhale_artist());
    assert!(app.funkwhale.tag.is_none());
    assert!(app.funkwhale.artist.is_some());
    assert_eq!(app.view.rows[0].title, "Track 2");
}

#[test]
fn selected_upload_owner_and_actual_date_are_distinct_from_artist_and_track_added_date() {
    let mut app = controller();
    let mut summary = track("pod.example", 1);
    summary.published_at = Some(1_704_164_645);
    complete(&mut app, vec![summary.clone()], None);
    let upload = FunkwhaleUploadMetadata {
        uploader_name: Some("uploader@other.example".to_owned()),
        uploader_url: Some(url::Url::parse("https://other.example/@uploader").unwrap()),
        created_at: Some(1_672_531_200),
        uploader_actor: None,
    };
    app.apply_funkwhale_reply(
        Job::Details {
            generation: app.funkwhale.generation,
            id: summary.video_id.clone(),
        },
        Ok(Reply::DetailsWithUpload(details(&summary), Some(upload))),
    );
    let view = app.view.details.as_ref().unwrap();
    assert!(
        view.description
            .contains("Uploaded by: uploader@other.example\nhttps://other.example/@uploader")
    );
    assert!(view.description.contains("Upload date: 2023 January 1"));
    assert_eq!(view.channel_name, "Fixture artist");
    assert_eq!(view.published, "2024 January 2");
    let raw = details(&summary);
    app.cache_funkwhale_details(raw, UploadMetadataUpdate::Unchanged);
    app.update_funkwhale_detail();
    let view = app.view.details.as_ref().unwrap();
    assert_eq!(view.description.matches("Uploaded by:").count(), 1);
    assert_eq!(view.description.matches("Upload date:").count(), 1);
    assert!(view.links.iter().any(|link| matches!(
        link.internal_target,
        Some(DetailLinkInternalTarget::FunkwhaleTag(_))
    )));
}

#[test]
fn missing_upload_details_do_not_invent_an_uploader_or_fail_the_track() {
    let mut app = controller();
    let summary = track("pod.example", 1);
    complete(&mut app, vec![summary.clone()], None);
    app.apply_funkwhale_reply(
        Job::Details {
            generation: app.funkwhale.generation,
            id: summary.video_id.clone(),
        },
        Ok(Reply::DetailsWithUpload(details(&summary), None)),
    );
    assert!(app.view.error_popup.is_none());
    assert!(
        !app.view
            .details
            .as_ref()
            .unwrap()
            .description
            .contains("Uploaded by:")
    );
    assert!(
        !app.view
            .details
            .as_ref()
            .unwrap()
            .description
            .contains("Upload date:")
    );
    let old_generation = app.funkwhale.generation;
    app.submit_funkwhale_search("different".to_owned());
    app.apply_funkwhale_reply(
        Job::Details {
            generation: old_generation,
            id: summary.video_id.clone(),
        },
        Ok(Reply::DetailsWithUpload(
            details(&summary),
            Some(FunkwhaleUploadMetadata {
                uploader_name: Some("stale".to_owned()),
                ..FunkwhaleUploadMetadata::default()
            }),
        )),
    );
    assert!(
        !app.funkwhale
            .upload_metadata
            .contains_key(&summary.video_id)
    );
}

#[test]
fn tag_actions_cannot_use_other_tabs_or_stale_unadvertised_values() {
    let mut app = controller();
    complete(&mut app, vec![track("pod.example", 1)], None);
    app.search_funkwhale_tag("unadvertised".to_owned());
    assert!(app.funkwhale.tag.is_none());
    app.view.screen = Screen::Local;
    app.search_funkwhale_tag("unadvertised".to_owned());
    assert_eq!(app.view.screen, Screen::Local);
    assert!(app.funkwhale.tag.is_none());
}

#[test]
fn playback_only_metadata_does_not_prevent_one_lazy_selected_upload_lookup() {
    let mut app = controller();
    let summary = track("pod.example", 1);
    complete(&mut app, vec![summary.clone()], None);
    app.cache_funkwhale_details(details(&summary), UploadMetadataUpdate::Unchanged);
    assert!(
        !app.funkwhale
            .upload_metadata_checked
            .contains(&summary.video_id)
    );
    let (release, gate) = bounded::<()>(1);
    let (held_sender, response) = bounded(1);
    let handle = thread::spawn(move || {
        let _ = gate.recv();
        drop(held_sender);
    });
    app.funkwhale.worker = Some(Worker {
        job: Job::Pods,
        response,
        thread: handle,
    });
    app.diagnostic_only = false;
    app.update_funkwhale_detail();
    assert!(
        app.funkwhale
            .jobs
            .iter()
            .any(|job| matches!(job, Job::Details { id, .. } if id == &summary.video_id))
    );
    app.diagnostic_only = true;
    app.funkwhale.jobs.clear();
    let old = app.funkwhale.worker.take().unwrap();
    release.send(()).unwrap();
    old.thread.join().unwrap();
    app.apply_funkwhale_reply(
        Job::Details {
            generation: app.funkwhale.generation,
            id: summary.video_id.clone(),
        },
        Ok(Reply::DetailsWithUpload(details(&summary), None)),
    );
    assert!(
        app.funkwhale
            .upload_metadata_checked
            .contains(&summary.video_id)
    );
}

#[test]
fn standalone_now_playing_snapshot_never_claims_to_be_a_complete_artist_catalogue() {
    let mut app = controller();
    let playing = track("pod.example", 1);
    complete(&mut app, vec![playing.clone()], None);
    app.open_funkwhale_artist();
    let snapshot = app
        .funkwhale_navigation_snapshot(&queue_item(&playing).unwrap())
        .unwrap();
    assert_eq!(snapshot.items.len(), 1);
    assert!(snapshot.artist.is_none());
    assert!(snapshot.next_page.is_none());
    let mut another = track("pod.example", 2);
    another.channel_id = "https://pod.example/library/artists/77".to_owned();
    complete(&mut app, vec![another], None);
    let snapshot = app
        .funkwhale_navigation_snapshot(&queue_item(&playing).unwrap())
        .unwrap();
    assert!(snapshot.artist.is_none());
}

#[test]
fn filtered_empty_page_keeps_an_advertised_later_page_available() {
    let mut app = controller();
    complete(&mut app, Vec::new(), Some(2));
    assert_eq!(app.funkwhale.next_page, Some(2));
    assert_eq!(app.view.rows[0].title, "Load more tracks...");
}

#[test]
fn resolver_queued_behind_metadata_has_immediate_animation_and_a_visible_failure() {
    let mut app = controller();
    let summary = track("pod.example", 1);
    complete(&mut app, vec![summary.clone()], None);
    let (release, gate) = bounded::<()>(1);
    let (held_sender, response) = bounded(1);
    let handle = thread::spawn(move || {
        let _ = gate.recv();
        drop(held_sender);
    });
    app.funkwhale.worker = Some(Worker {
        job: Job::Details {
            generation: app.funkwhale.generation,
            id: summary.video_id.clone(),
        },
        response,
        thread: handle,
    });
    app.diagnostic_only = false;
    app.dispatch(UiAction::ActivateSelection);
    assert!(app.funkwhale_playback_pending());
    assert!(app.view.playback_preparing);
    assert!(app.view.needs_animation_tick());
    assert!(app.current_media.is_none());
    let old = app.funkwhale.worker.take().unwrap();
    release.send(()).unwrap();
    old.thread.join().unwrap();
    let job = app.funkwhale.jobs.pop_front().unwrap();
    assert!(matches!(job, Job::Playback { .. }));
    let (sender, response) = bounded(1);
    sender
        .send(Err("This track has no accessible audio upload".to_owned()))
        .unwrap();
    let handle = thread::spawn(|| {});
    app.funkwhale.worker = Some(Worker {
        job,
        response,
        thread: handle,
    });
    app.poll_funkwhale_worker();
    app.refresh_playback_preparation_activity();
    assert!(!app.view.playback_preparing);
    assert!(!app.funkwhale_playback_pending());
    let popup = app
        .view
        .error_popup
        .as_ref()
        .expect("TTY-visible playback failure");
    assert!(popup.report.contains("no accessible audio upload"));
    assert!(popup.report.contains(&summary.video_id));
}

#[test]
fn autoplay_retains_source_order_after_a_later_search() {
    let mut app = controller();
    complete(
        &mut app,
        vec![track("pod.example", 1), track("pod.example", 2)],
        None,
    );
    let id = app.view.rows[0].media_id.clone().unwrap();
    let origin = app.funkwhale_autoplay_origin(&id).unwrap();
    app.submit_funkwhale_search("replacement".to_owned());
    let AutoplayOrigin::Funkwhale { items, index } = origin else {
        panic!("wrong source")
    };
    assert_eq!(index, 0);
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].media.title, "Track 2");
}

#[test]
fn retained_now_playing_reveal_and_back_restore_the_real_source_list() {
    let mut app = controller();
    complete(&mut app, vec![track("pod.example", 1)], None);
    let playing = queue_item(&track("pod.example", 1)).unwrap();
    app.current_media = Some(playing.media.id.clone());
    app.remember_now_playing_context(&playing);
    app.submit_funkwhale_search("replacement".to_owned());
    complete(
        &mut app,
        vec![track("pod.example", 2), track("pod.example", 3)],
        None,
    );
    app.show_screen(Screen::Search);
    assert!(app.reveal_retained_now_playing(&playing));
    assert_eq!(app.view.screen, Screen::Funkwhale);
    assert_eq!(app.view.rows[0].title, "Track 1");
    assert!(app.restore_now_playing_location());
    assert_eq!(app.view.rows[0].title, "Track 2");
    assert_eq!(app.view.search_query, "replacement");
}

#[test]
fn shutdown_revokes_queued_work_and_startup_searches() {
    let mut app = controller();
    app.funkwhale.restore = true;
    app.funkwhale.jobs.push_back(Job::Pods);
    let generation = app.funkwhale.generation;
    app.shutdown_funkwhale();
    assert!(!app.funkwhale.restore);
    assert!(app.funkwhale.jobs.is_empty());
    assert_ne!(app.funkwhale.generation, generation);
}

#[test]
fn client_cache_reuses_the_owner_and_remains_bounded_without_network_access() {
    let mut app = controller();
    for number in 0..8 {
        let job = Job::Details {
            generation: 1,
            id: track(&format!("pod{number}.example"), 1).video_id,
        };
        let first = app.funkwhale_client_for_job(&job).unwrap().unwrap();
        let second = app.funkwhale_client_for_job(&job).unwrap().unwrap();
        assert_eq!(first.base_url(), second.base_url());
        assert_eq!(
            app.funkwhale
                .clients
                .iter()
                .filter(|client| client.base_url() == first.base_url())
                .count(),
            1
        );
        assert!(app.funkwhale.clients.len() <= 4);
    }
    assert!(app.funkwhale_client_for_job(&Job::Pods).unwrap().is_none());
}

#[test]
fn previous_track_eof_preserves_the_new_explicit_resolver_and_queue_slot() {
    for queued in [false, true] {
        let mut app = controller();
        let ended = queue_item(&track("pod.example", 1)).unwrap();
        let requested = queue_item(&track("pod.example", 2)).unwrap();
        app.playback_queue.push(ended.clone());
        app.playback_queue.push(requested.clone());
        app.playback_queue.current_index = Some(usize::from(queued));
        app.current_media = Some(ended.media.id.clone());
        app.playback_phase = PlaybackPhase::Playing;
        app.funkwhale.jobs.push_back(Job::Playback {
            generation: app.funkwhale.playback_generation,
            item: Box::new(requested),
            queued,
            origin: None,
        });
        let generation = app.funkwhale.playback_generation;
        app.handle_playback_end(
            PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            },
            Duration::ZERO,
        );
        assert_eq!(
            app.funkwhale.playback_generation, generation,
            "old EOF must not cancel a newer intent"
        );
        assert_eq!(
            app.playback_queue.current_index,
            Some(1),
            "old EOF must not skip the requested or unplayed slot"
        );
        assert!(
            app.funkwhale
                .jobs
                .iter()
                .any(|job| matches!(job, Job::Playback { .. }))
        );
    }
}

#[cfg(feature = "bbc-radio")]
#[test]
fn a_new_funkwhale_intent_immediately_cancels_an_older_bbc_resolver() {
    let mut app = controller();
    let item = queue_item(&track("pod.example", 1)).unwrap();
    app.pending_bbc_playback = Some(PendingBbcPlayback {
        generation: app.bbc_playback_generation,
        item: item.clone(),
        queue_cursor_already_positioned: false,
        origin: None,
    });
    let generation = app.bbc_playback_generation;
    app.request_funkwhale_playback(item, false, None);
    assert!(app.pending_bbc_playback.is_none());
    assert_ne!(app.bbc_playback_generation, generation);
}

#[cfg(feature = "yandex-music")]
#[test]
fn a_new_yandex_intent_immediately_cancels_an_older_funkwhale_resolver() {
    let mut app = controller();
    let item = queue_item(&track("pod.example", 1)).unwrap();
    app.funkwhale.jobs.push_back(Job::Playback {
        generation: app.funkwhale.playback_generation,
        item: Box::new(item.clone()),
        queued: false,
        origin: None,
    });
    let generation = app.funkwhale.playback_generation;
    let track = yandex_music_track_from_queue_item(&item);
    app.request_yandex_music_playback(track, item, false, None);
    assert_ne!(app.funkwhale.playback_generation, generation);
    assert!(!app.funkwhale_playback_pending());
}

/// A backend spy observes the resolved input, never the stable queue locator.
struct RecordingBackend(Arc<std::sync::Mutex<Vec<PlaybackInput>>>);

impl PlaybackBackend for RecordingBackend {
    fn play(&mut self, input: &PlaybackInput) -> crate::playback::Result<()> {
        self.0.lock().unwrap().push(input.clone());
        Ok(())
    }
    fn command(&mut self, _command: PlayerCommand) -> crate::playback::Result<()> {
        Ok(())
    }
    fn status(&mut self) -> crate::playback::Result<PlaybackStatus> {
        Ok(PlaybackStatus::default())
    }
    fn shutdown(&mut self) -> crate::playback::Result<()> {
        Ok(())
    }
}

#[test]
fn accepted_audio_uses_the_backend_but_keeps_canonical_queue_metadata() {
    let mut app = controller();
    let inputs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&inputs);
    app.playback_factory = Some(Box::new(move || {
        Ok(Box::new(RecordingBackend(Arc::clone(&recorded))))
    }));
    let summary = track("pod.example", 1);
    let item = queue_item(&summary).unwrap();
    complete(&mut app, vec![summary.clone()], None);
    app.apply_funkwhale_reply(
        Job::Playback {
            generation: app.funkwhale.playback_generation,
            item: Box::new(item.clone()),
            queued: false,
            origin: app.funkwhale_autoplay_origin(&item.media.id),
        },
        Ok(Reply::Playback(
            details(&summary),
            summary.stream_url.clone().unwrap(),
        )),
    );
    let inputs = inputs.lock().unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].location, summary.stream_url.unwrap().as_str());
    assert!(inputs[0].bypass_ytdl);
    assert_eq!(app.current_media.as_ref(), Some(&item.media.id));
    assert_eq!(app.view.playing_screen, Some(Screen::Funkwhale));
    assert_eq!(
        app.playback_queue.current().unwrap().playback_location,
        item.media.webpage_url.as_str()
    );
    assert!(
        !serde_json::to_string(&app.playback_queue)
            .unwrap()
            .contains("transient")
    );
}

#[test]
fn switching_tabs_retains_an_unsubmitted_query_without_a_hidden_spinner() {
    let mut app = controller();
    app.view.search_query = "unfinished query".to_owned();
    app.begin_search_activity(SearchActivity::Funkwhale);
    app.show_screen(Screen::Search);
    assert_ne!(app.view.search_activity, Some(SearchActivity::Funkwhale));
    app.show_screen(Screen::Funkwhale);
    assert_eq!(app.view.search_query, "unfinished query");
}

#[test]
fn restart_defers_saved_search_until_its_tab_is_visible_and_respects_privacy() {
    for history in [true, false] {
        let store = StateStore::open_in_memory().unwrap();
        store
            .save_session(
                &SessionState {
                    screen: StoredScreen::Funkwhale,
                    funkwhale_search_text: "saved query".to_owned(),
                    funkwhale_selected_row: Some(3),
                    ..SessionState::default()
                },
                1,
            )
            .unwrap();
        let mut config = Config::for_dir("/tmp/youta-funkwhale-restart-fixture");
        config.providers.funkwhale_instance_url =
            Some(url::Url::parse("https://pod.example/").unwrap());
        config.persistence.save_playback_history = history;
        let mut app = AppController::new(config, store, None, None);
        app.diagnostic_only = true;
        assert!(app.funkwhale.worker.is_none());
        if history {
            assert_eq!(app.view.search_query, "saved query");
            assert!(app.funkwhale.restore);
            app.show_screen(Screen::Search);
            app.poll_funkwhale_worker();
            assert!(app.funkwhale.restore);
            app.show_screen(Screen::Funkwhale);
            app.poll_funkwhale_worker();
            assert!(!app.funkwhale.restore);
            assert_eq!(app.funkwhale.selected, 3);
            assert_eq!(app.view.search_activity, Some(SearchActivity::Funkwhale));
        } else {
            assert!(app.funkwhale.query.is_empty());
            assert!(app.view.search_query.is_empty());
            assert!(!app.funkwhale.restore);
        }
    }
}
