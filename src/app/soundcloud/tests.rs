//! Offline controller coverage for Soundcloak navigation and canonical replay.

use super::*;
use std::sync::Mutex;

/// Builds public metadata without contacting any real instance.
pub(super) fn track(slug: &str) -> SoundcloakTrack {
    SoundcloakTrack {
        id: slug.to_owned(),
        title: format!("Track {slug}"),
        artist: "Fixture Artist".to_owned(),
        artist_url: None,
        webpage_url: url::Url::parse(&format!("https://soundcloud.com/fixture-artist/{slug}"))
            .unwrap(),
        artwork_url: None,
        expanded_artwork_url: None,
        duration_seconds: Some(120),
        full_duration_seconds: Some(120),
        description: Some("Fixture description".to_owned()),
        streamable: true,
        playback: crate::providers::soundcloak::SoundcloakPlayback::Full,
        likes_count: None,
        playback_count: None,
        reposts_count: None,
        comment_count: None,
        created_at: None,
        last_modified: None,
        license: None,
        tags: Vec::new(),
        genre: None,
    }
}

/// Uses in-memory persistence and an explicit test instance, with no live network.
pub(super) fn controller() -> AppController {
    let mut config = Config::for_dir("/tmp/youta-soundcloud-controller-test");
    config.providers.soundcloak_base_url =
        Some(url::Url::parse("https://soundcloak.example/").unwrap());
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    controller.show_screen(Screen::SoundCloud);
    controller
}

/// Supplies a completion without creating a worker or fetching media.
pub(super) fn complete(
    controller: &mut AppController,
    generation: u64,
    items: Vec<SoundcloakTrack>,
    next_page: Option<u32>,
) {
    controller.apply_soundcloud_page(
        SearchJob {
            generation,
            tag: None,
            request: SoundcloakSearchRequest {
                query: "fixture".to_owned(),
                page: 1,
                limit: 50,
                query_urn: None,
            },
        },
        Ok(SoundcloakSearchPage {
            items,
            page: 1,
            next_page,
            query_urn: Some("urn:fixture".to_owned()),
        }),
    );
}

/// Holds each mock HTTP request until the test releases it, recording concurrency and limits.
struct GatedSoundcloakTransport {
    started: Sender<String>,
    release: Receiver<()>,
    requests: Mutex<Vec<(url::Url, usize)>>,
    active: std::sync::atomic::AtomicUsize,
    peak_active: std::sync::atomic::AtomicUsize,
}

impl crate::providers::soundcloak::SoundcloakTransport for GatedSoundcloakTransport {
    fn fetch(
        &self,
        url: &url::Url,
        max_bytes: usize,
    ) -> Result<Vec<u8>, crate::providers::ProviderError> {
        use std::sync::atomic::Ordering;

        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_active.fetch_max(active, Ordering::SeqCst);
        self.requests.lock().unwrap().push((url.clone(), max_bytes));
        let result = (|| {
            let query = url
                .query_pairs()
                .find_map(|(key, value)| (key == "q").then(|| value.into_owned()))
                .expect("search request contains its query");
            self.started
                .send_timeout(query.clone(), Duration::from_secs(5))
                .map_err(|error| crate::providers::ProviderError::Transport(error.to_string()))?;
            self.release
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| crate::providers::ProviderError::Transport(error.to_string()))?;
            Ok(serde_json::to_vec(&serde_json::json!({
                "collection": [{
                    "id": 1,
                    "kind": "track",
                    "title": format!("Track {query}"),
                    "user": {"username": "Fixture Artist"},
                    "permalink_url": format!("https://soundcloud.com/fixture-artist/{query}"),
                    "duration": 120_000,
                    "policy": "ALLOW",
                    "streamable": true,
                    "media": {"transcodings": [{
                        "snipped": false,
                        "format": {"protocol": "hls", "mime_type": "audio/mpeg"}
                    }]}
                }],
                "next_href": null
            }))
            .unwrap())
        })();
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

/// Waits only for a released mock request, leaving response application to the controller.
fn wait_for_soundcloud_worker(controller: &AppController) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !controller
        .soundcloud
        .worker
        .as_ref()
        .expect("one mock request is running")
        .thread
        .is_finished()
    {
        assert!(Instant::now() < deadline, "released mock worker completed");
        thread::yield_now();
    }
}

/// Rapid searches retain one worker and only the newest pending request across tab changes.
#[test]
fn soundcloud_worker_coalesces_queries_and_preserves_other_tab_activity() {
    use std::sync::atomic::Ordering;

    let (started, requests_started) = bounded(4);
    let (release, releases) = bounded(1);
    let transport = Arc::new(GatedSoundcloakTransport {
        started,
        release: releases,
        requests: Mutex::new(Vec::new()),
        active: std::sync::atomic::AtomicUsize::new(0),
        peak_active: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut controller = controller();
    controller.soundcloud.client = Some(
        SoundcloakClient::with_transport(
            url::Url::parse("https://soundcloak.example/").unwrap(),
            transport.clone(),
        )
        .unwrap(),
    );
    controller.view.search_query = "a".to_owned();
    controller.submit_search();
    assert_eq!(
        requests_started
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        "a"
    );
    assert_eq!(
        controller.view.search_activity,
        Some(SearchActivity::SoundCloud)
    );

    for query in ["b", "c"] {
        controller.view.search_query = query.to_owned();
        controller.submit_search();
    }
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert_eq!(
        controller
            .soundcloud
            .worker
            .as_ref()
            .unwrap()
            .job
            .request
            .query,
        "a"
    );
    assert_eq!(
        controller
            .soundcloud
            .pending
            .as_ref()
            .unwrap()
            .request
            .query,
        "c"
    );

    controller.show_screen(Screen::Search);
    controller.view.rows = vec![RowView {
        title: "Another tab's result".to_owned(),
        ..RowView::default()
    }];
    controller.view.details = Some(DetailView {
        title: "Another tab's details".to_owned(),
        ..DetailView::default()
    });
    controller.view.status_line = "Another search is pending".to_owned();
    controller.begin_search_activity(SearchActivity::YouTube);
    controller.view.search_animation_frame = 7;
    let rows = controller.view.rows.clone();
    let details = controller.view.details.clone();
    let status = controller.view.status_line.clone();

    release.send(()).unwrap();
    wait_for_soundcloud_worker(&controller);
    controller.poll_soundcloud_worker();
    assert_eq!(
        requests_started
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        "c",
        "the queued b query must never reach the transport"
    );
    assert!(
        controller.soundcloud.items.is_empty(),
        "stale a must not populate results"
    );
    assert!(controller.soundcloud.pending.is_none());
    assert_eq!(controller.view.rows, rows);
    assert_eq!(controller.view.details, details);
    assert_eq!(controller.view.status_line, status);
    assert_eq!(
        controller.view.search_activity,
        Some(SearchActivity::YouTube)
    );
    assert_eq!(controller.view.search_animation_frame, 7);

    release.send(()).unwrap();
    wait_for_soundcloud_worker(&controller);
    controller.poll_soundcloud_worker();
    assert!(controller.soundcloud.worker.is_none());
    assert!(controller.soundcloud.pending.is_none());
    assert_eq!(controller.soundcloud.items.len(), 1);
    assert_eq!(controller.soundcloud.items[0].title, "Track c");
    assert_eq!(controller.view.screen, Screen::Search);
    assert_eq!(controller.view.rows, rows);
    assert_eq!(controller.view.details, details);
    assert_eq!(controller.view.status_line, status);
    assert_eq!(
        controller.view.search_activity,
        Some(SearchActivity::YouTube)
    );
    assert_eq!(controller.view.search_animation_frame, 7);
    assert!(controller.playback_queue.items.is_empty());
    assert_eq!(transport.active.load(Ordering::SeqCst), 0);
    assert_eq!(transport.peak_active.load(Ordering::SeqCst), 1);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (url, max_bytes) in requests.iter() {
        assert_eq!(url.host_str(), Some("soundcloak.example"));
        assert_eq!(url.path(), "/_/api/v2/search/tracks");
        let query = url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(query.get("limit").map(|value| value.as_ref()), Some("50"));
        assert_eq!(query.get("offset").map(|value| value.as_ref()), Some("0"));
        assert_eq!(*max_bytes, crate::providers::DEFAULT_MAX_JSON_BYTES);
    }
}

#[test]
fn soundcloud_details_show_both_original_and_instance_page_links() {
    let mut controller = controller();
    complete(&mut controller, 0, vec![track("one")], None);
    let details = controller.view.details.as_ref().unwrap();
    assert_eq!(details.links.len(), 2);
    assert_eq!(
        details.links[0].url,
        "https://soundcloud.com/fixture-artist/one"
    );
    assert_eq!(
        details.links[1].url,
        "https://soundcloak.example/fixture-artist/one"
    );
    assert_eq!(
        controller.current_url().as_deref(),
        Some("https://soundcloud.com/fixture-artist/one")
    );
}

#[test]
fn soundcloud_details_preserve_source_facts_genre_and_lazy_artwork_sizes() {
    let mut app = controller();
    let mut item = track("metadata");
    item.likes_count = Some(1234);
    item.playback_count = Some(5678);
    item.reposts_count = Some(90);
    item.comment_count = Some(12);
    item.created_at = Some("2024-03-02T10:20:30Z".into());
    item.last_modified = Some("2025-01-04T11:22:33Z".into());
    item.license = Some("cc-by".into());
    item.tags = vec!["ambient".into(), "field recording".into()];
    item.genre = Some("Ambient & Field".into());
    item.artwork_url = Some(url::Url::parse("https://soundcloak.example/artwork-500").unwrap());
    item.expanded_artwork_url =
        Some(url::Url::parse("https://soundcloak.example/artwork-1080").unwrap());
    complete(&mut app, 0, vec![item.clone()], None);
    let details = app.view.details.as_ref().unwrap();
    assert_eq!(details.likes, "1,234");
    assert_eq!(details.comments, "12");
    assert_eq!(details.license, "cc-by");
    assert!(details.views.is_empty(), "plays are not video views");
    assert!(details.published.is_empty(), "creation is not publication");
    let facts = details.soundcloud.as_ref().unwrap();
    assert_eq!((facts.plays, facts.reposts), (Some(5678), Some(90)));
    assert!(facts.created.contains("2024"));
    assert!(facts.modified.contains("2025"));
    assert_eq!(facts.tags, ["ambient", "field recording"]);
    assert_eq!(details.thumbnail_url, item.artwork_url);
    assert_eq!(details.expanded_thumbnail_url, item.expanded_artwork_url);
    assert!(!details.thumbnail_expanded);
    let genre = details
        .links
        .iter()
        .find(|link| link.prefix == "Genre: ")
        .unwrap();
    assert_eq!(genre.label, "Ambient & Field");
    assert!(genre.url.starts_with("https://soundcloak.example/tags/"));
}

#[test]
fn soundcloud_preview_rows_and_details_never_claim_full_length_playback() {
    let mut app = controller();
    let mut item = track("preview");
    item.playback = crate::providers::soundcloak::SoundcloakPlayback::Preview;
    item.duration_seconds = Some(30);
    item.full_duration_seconds = Some(180);
    complete(&mut app, 0, vec![item], None);
    assert!(app.view.rows[0].subtitle.contains("preview"));
    let details = app.view.details.as_ref().unwrap();
    assert_eq!(details.length, "0:30 preview (full track 3:00)");
    assert_eq!(
        details
            .soundcloud
            .as_ref()
            .unwrap()
            .preview_duration_seconds,
        Some(30)
    );
    assert!(
        app.selected_soundcloud_queue_item().is_ok(),
        "public previews are explicit playable choices"
    );
}

#[cfg(feature = "wikidata")]
#[test]
fn soundcloud_detail_rebuild_preserves_same_track_wikidata_links_and_spoilers() {
    let mut app = controller();
    complete(&mut app, 0, vec![track("one"), track("two")], None);
    let details = app.view.details.as_mut().unwrap();
    details.wikidata = "Fixture artist (Q123)".into();
    details.links.push(DetailLinkView {
        label: "Fixture artist".into(),
        url: "https://www.wikidata.org/wiki/Q123".into(),
        wikidata_item_id: Some("Q123".into()),
        ..DetailLinkView::default()
    });
    details.expanded_wikidata_item = Some("Q123".into());
    details.loading_wikidata_item = Some("Q123".into());
    details
        .wikidata_entities
        .push(crate::view::DetailWikidataEntityView {
            item_id: "Q123".into(),
            text: "Fixture artist properties".into(),
            ..crate::view::DetailWikidataEntityView::default()
        });
    for _ in 0..2 {
        app.update_soundcloud_detail();
        let details = app.view.details.as_ref().unwrap();
        assert_eq!(details.wikidata, "Fixture artist (Q123)");
        assert_eq!(
            details
                .links
                .iter()
                .filter(|link| link.wikidata_item_id.as_deref() == Some("Q123"))
                .count(),
            1
        );
        assert_eq!(details.expanded_wikidata_item.as_deref(), Some("Q123"));
        assert_eq!(details.loading_wikidata_item.as_deref(), Some("Q123"));
        assert_eq!(details.wikidata_entities.len(), 1);
    }
    app.select_row(1);
    let details = app.view.details.as_ref().unwrap();
    assert!(details.wikidata_entities.is_empty());
    assert!(
        details
            .links
            .iter()
            .all(|link| link.wikidata_item_id.is_none())
    );
}

#[test]
fn soundcloud_results_ignore_stale_completions_and_do_not_replace_another_tab() {
    let mut controller = controller();
    controller.soundcloud.generation = 2;
    complete(&mut controller, 1, vec![track("stale")], None);
    assert!(controller.soundcloud.items.is_empty());
    controller.show_screen(Screen::Search);
    let rows = controller.view.rows.clone();
    complete(&mut controller, 2, vec![track("current")], Some(2));
    assert_eq!(controller.view.rows, rows);
    assert_eq!(controller.soundcloud.items.len(), 1);
    controller.show_screen(Screen::SoundCloud);
    assert_eq!(controller.view.rows.len(), 2);
    assert_eq!(controller.view.rows[1].title, "Load more tracks…");
    controller.select_row(1);
    assert!(controller.selected_queue_item().is_err());
    assert!(controller.view.details.is_none());
}

#[test]
fn soundcloud_tabs_keep_queries_selection_and_session_independent() {
    let mut controller = controller();
    complete(&mut controller, 0, vec![track("one"), track("two")], None);
    controller.view.search_query = "ambient".to_owned();
    controller.select_row(1);
    controller.show_screen(Screen::YouTubeMusic);
    controller.view.search_query = "classical".to_owned();
    controller.show_screen(Screen::SoundCloud);
    assert_eq!(controller.view.search_query, "ambient");
    assert_eq!(controller.view.selected, 1);
    assert!(controller.save_session());
    let saved = controller.store.session().unwrap().unwrap();
    assert_eq!(saved.screen, StoredScreen::SoundCloud);
    assert_eq!(saved.soundcloud_search_text, "ambient");
    assert_eq!(saved.youtube_music_search_text, "classical");
    assert_eq!(saved.soundcloud_selected_row, Some(1));
}

#[test]
fn soundcloud_saved_track_and_runtime_stream_have_separate_urls() {
    let mut controller = controller();
    complete(&mut controller, 0, vec![track("one")], None);
    let item = controller.selected_queue_item().unwrap();
    let snapshot = controller.selected_playlist_snapshot().unwrap();
    assert_eq!(
        snapshot.replay_locator,
        "https://soundcloud.com/fixture-artist/one"
    );
    assert_eq!(item.media.id.external_id, snapshot.replay_locator);
    let input = controller
        .soundcloak_client()
        .unwrap()
        .playback_url(&track("one"))
        .unwrap();
    assert!(
        input
            .as_str()
            .starts_with("https://soundcloak.example/_/api/hls/fixture-artist/one?")
    );
    assert!(input.as_str().contains("redirect_parts=false"));
    assert!(!snapshot.replay_locator.contains("soundcloak"));
}

#[test]
fn soundcloud_autoplay_snapshot_survives_search_and_skips_unavailable_tracks() {
    let mut controller = controller();
    let mut unavailable = track("unavailable");
    unavailable.streamable = false;
    complete(
        &mut controller,
        0,
        vec![track("one"), unavailable, track("two")],
        None,
    );
    let item = controller.selected_queue_item().unwrap();
    let origin = controller
        .autoplay_origin_for_media(&item.media.id)
        .unwrap();
    controller.soundcloud.items.clear();
    let AutoplayOrigin::SoundCloud { items, index } = origin else {
        panic!("SoundCloud origin expected");
    };
    assert_eq!(index, 0);
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].media.title, "Track two");
}

#[test]
fn soundcloud_duplicate_continuations_stop_pagination() {
    let mut controller = controller();
    complete(&mut controller, 0, vec![track("one")], Some(2));
    complete(&mut controller, 0, vec![track("one")], Some(3));
    assert_eq!(controller.soundcloud.items.len(), 1);
    assert_eq!(controller.soundcloud.next_page, None);
}

/// Returns exact consecutive pages from the real provider's validated offset contract.
#[derive(Default)]
struct PagingSoundcloakTransport(Mutex<Vec<url::Url>>);

impl crate::providers::soundcloak::SoundcloakTransport for PagingSoundcloakTransport {
    fn fetch(&self, url: &url::Url, _: usize) -> Result<Vec<u8>, crate::providers::ProviderError> {
        self.0.lock().unwrap().push(url.clone());
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();
        let limit = pairs["limit"].parse::<usize>().unwrap();
        let offset = pairs["offset"].parse::<usize>().unwrap();
        let items = (offset..offset + limit)
            .map(|index| {
                serde_json::json!({
                    "id": index + 1,
                    "kind": "track",
                    "title": format!("Track {index}"),
                    "user": {"username": "Fixture Artist"},
                    "permalink_url": format!("https://soundcloud.com/fixture-artist/track-{index}"),
                    "duration": 120_000,
                    "policy": "ALLOW",
                    "streamable": true,
                    "media": {"transcodings": [{
                        "snipped": false,
                        "format": {"protocol": "hls", "mime_type": "audio/mpeg"}
                    }]}
                })
            })
            .collect::<Vec<_>>();
        let mut next = url::Url::parse("https://api-v2.soundcloud.com/search/tracks").unwrap();
        next.query_pairs_mut()
            .append_pair("q", &pairs["q"])
            .append_pair("limit", &limit.to_string())
            .append_pair("offset", &(offset + limit).to_string());
        Ok(serde_json::to_vec(&serde_json::json!({
            "collection": items,
            "next_href": next,
        }))
        .unwrap())
    }
}

/// Installs a recording in-memory transport; no test contacts a public instance.
fn paging_controller() -> (AppController, Arc<PagingSoundcloakTransport>) {
    let mut app = controller();
    let transport = Arc::new(PagingSoundcloakTransport::default());
    app.soundcloud.client = Some(
        SoundcloakClient::with_transport(
            url::Url::parse("https://soundcloak.example/").unwrap(),
            transport.clone(),
        )
        .unwrap(),
    );
    (app, transport)
}

/// Reopens only persisted navigation, installing mock HTTP before the first UI tick.
fn restored_soundcloud_controller(query: &str) -> (AppController, Arc<PagingSoundcloakTransport>) {
    restored_soundcloud_controller_on_screen(query, StoredScreen::SoundCloud)
}

/// Leaves a saved SoundCloud query dormant when startup restores a different tab.
fn restored_soundcloud_controller_on_screen(
    query: &str,
    screen: StoredScreen,
) -> (AppController, Arc<PagingSoundcloakTransport>) {
    let mut config = Config::for_dir("/tmp/youta-soundcloud-restored-search-test");
    config.providers.soundcloak_base_url =
        Some(url::Url::parse("https://soundcloak.example/").unwrap());
    let store = StateStore::open_in_memory().unwrap();
    store
        .save_session(
            &SessionState {
                screen,
                soundcloud_search_text: query.to_owned(),
                soundcloud_selected_row: Some(2),
                selected_row: 2,
                ..SessionState::default()
            },
            1,
        )
        .unwrap();
    let mut app = AppController::new(config, store, None, None);
    assert!(
        app.soundcloud.worker.is_none(),
        "construction must wait for viewport geometry"
    );
    let transport = Arc::new(PagingSoundcloakTransport::default());
    app.soundcloud.client = Some(
        SoundcloakClient::with_transport(
            url::Url::parse("https://soundcloak.example/").unwrap(),
            transport.clone(),
        )
        .unwrap(),
    );
    (app, transport)
}

#[test]
fn soundcloud_restored_query_searches_once_after_viewport_and_keeps_selection() {
    let (mut app, requests) = restored_soundcloud_controller("minsk");
    assert_eq!(app.view.search_query, "minsk");
    assert_eq!(
        app.view.search_activity,
        Some(SearchActivity::SoundCloud),
        "unrequested saved text is not a completed empty search"
    );
    assert!(requests.0.lock().unwrap().is_empty());
    app.set_soundcloud_search_page_capacity(10);
    app.populate_soundcloud();
    app.show_screen(Screen::SoundCloud);
    assert!(
        requests.0.lock().unwrap().is_empty(),
        "redraw and tab click must not start HTTP"
    );
    app.tick();
    assert!(
        app.soundcloud.worker.is_some(),
        "the first tick must schedule the saved query"
    );
    finish_soundcloud_page(&mut app);
    assert_eq!(app.view.rows.len(), 11);
    assert_eq!(app.view.selected, 2);
    assert_eq!(app.view.search_activity, None);
    for _ in 0..3 {
        app.populate_soundcloud();
        app.show_screen(Screen::SoundCloud);
        app.tick();
    }
    let urls = requests.0.lock().unwrap();
    assert_eq!(urls.len(), 1);
    let query = urls[0].query_pairs().collect::<HashMap<_, _>>();
    assert_eq!(query["q"], "minsk");
    assert_eq!(query["offset"], "0");
    assert_eq!(query["limit"], "10");
}

#[test]
fn soundcloud_restored_query_respects_newer_navigation_or_input() {
    for change in [
        "empty",
        "tab",
        "edit",
        "different query",
        "diagnostic",
        "quit",
    ] {
        let (mut app, requests) =
            restored_soundcloud_controller(if change == "empty" { " " } else { "minsk" });
        match change {
            "tab" => app.show_screen(Screen::Search),
            "edit" => app.view.search_editing = true,
            "different query" => app.view.search_query = "new draft".into(),
            "diagnostic" => app.diagnostic_only = true,
            "quit" => app.view.quitting = true,
            _ => {}
        }
        app.tick();
        assert!(app.soundcloud.worker.is_none(), "{change}");
        assert!(requests.0.lock().unwrap().is_empty(), "{change}");
    }
}

#[test]
fn soundcloud_manual_search_replaces_the_unstarted_saved_query() {
    let (mut app, requests) = restored_soundcloud_controller("minsk");
    app.set_soundcloud_search_page_capacity(8);
    app.view.search_query = "manual query".into();
    app.submit_soundcloud_search("manual query".into());
    finish_soundcloud_page(&mut app);
    app.tick();
    assert_eq!(app.view.selected, 0);
    let urls = requests.0.lock().unwrap();
    assert_eq!(urls.len(), 1);
    assert!(
        urls[0]
            .query_pairs()
            .any(|(key, value)| key == "q" && value == "manual query")
    );
}

#[test]
fn soundcloud_restored_query_waits_for_visible_tab_and_supports_no_geometry_hint() {
    for starts_hidden in [false, true] {
        let (mut app, requests) = restored_soundcloud_controller_on_screen(
            "minsk",
            if starts_hidden {
                StoredScreen::Search
            } else {
                StoredScreen::SoundCloud
            },
        );
        if !starts_hidden {
            app.show_screen(Screen::Search);
        }
        app.tick();
        assert!(requests.0.lock().unwrap().is_empty());
        assert!(app.soundcloud.worker.is_none());
        app.show_screen(Screen::SoundCloud);
        app.tick();
        assert!(app.soundcloud.worker.is_some());
        finish_soundcloud_page(&mut app);
        let urls = requests.0.lock().unwrap();
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0]
                .query_pairs()
                .any(|(key, value)| key == "limit" && value == "50"),
            "non-terminal frontend uses the bounded fallback"
        );
    }
}

/// Returns terminal search outcomes without contacting an instance or following continuations.
struct RestoredTerminalOutcomeTransport {
    requests: std::sync::atomic::AtomicUsize,
    fail: bool,
}

impl crate::providers::soundcloak::SoundcloakTransport for RestoredTerminalOutcomeTransport {
    fn fetch(&self, _: &url::Url, _: usize) -> Result<Vec<u8>, crate::providers::ProviderError> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail {
            Err(crate::providers::ProviderError::Transport(
                "fixture search failure".into(),
            ))
        } else {
            Ok(br#"{"collection":[],"next_href":null}"#.to_vec())
        }
    }
}

#[test]
fn soundcloud_restored_query_empty_or_error_never_retries_on_redraw_or_tab_return() {
    for fail in [false, true] {
        let (mut app, _) = restored_soundcloud_controller("minsk");
        let transport = Arc::new(RestoredTerminalOutcomeTransport {
            requests: std::sync::atomic::AtomicUsize::new(0),
            fail,
        });
        app.soundcloud.client = Some(
            SoundcloakClient::with_transport(
                url::Url::parse("https://soundcloak.example/").unwrap(),
                transport.clone(),
            )
            .unwrap(),
        );
        app.tick();
        assert!(app.soundcloud.worker.is_some());
        finish_soundcloud_page(&mut app);
        assert!(app.view.rows.is_empty());
        assert_eq!(app.view.search_activity, None);
        if fail {
            assert!(app.view.status_line.contains("fixture search failure"));
        }
        for _ in 0..3 {
            app.populate_soundcloud();
            app.tick();
            app.show_screen(Screen::Search);
            app.show_screen(Screen::SoundCloud);
            app.tick();
        }
        assert_eq!(
            transport.requests.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(app.soundcloud.worker.is_none());
    }
}

/// Completes one immediately available mock page without starting a second request.
fn finish_soundcloud_page(app: &mut AppController) {
    wait_for_soundcloud_worker(app);
    app.poll_soundcloud_worker();
    assert!(app.soundcloud.worker.is_none());
}

#[test]
fn soundcloud_first_request_uses_bounded_visible_capacity() {
    for (capacity, expected) in [(10, 10), (100, 100), (0, 1), (usize::MAX, 100)] {
        let (mut app, requests) = paging_controller();
        app.set_soundcloud_search_page_capacity(capacity);
        assert!(
            requests.0.lock().unwrap().is_empty(),
            "geometry must not fetch"
        );
        app.submit_soundcloud_search("fixture".into());
        finish_soundcloud_page(&mut app);
        assert_eq!(app.soundcloud.items.len(), expected);
        assert_eq!(app.view.rows.len(), expected + 1);
        assert_eq!(app.view.rows.last().unwrap().title, "Load more tracks…");
        let urls = requests.0.lock().unwrap();
        assert_eq!(urls.len(), 1);
        let pairs = urls[0].query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(pairs["limit"], expected.to_string());
        assert_eq!(pairs["offset"], "0");
    }
}

#[test]
fn soundcloud_load_more_turns_one_page_and_keeps_its_stride_after_resize() {
    let (mut app, requests) = paging_controller();
    app.set_soundcloud_search_page_capacity(10);
    app.submit_soundcloud_search("fixture".into());
    finish_soundcloud_page(&mut app);
    app.set_soundcloud_search_page_capacity(100);
    app.view.search_query = "unsubmitted draft".into();
    for page in 2..=4 {
        app.select_row(app.soundcloud.items.len());
        app.activate_soundcloud_selection();
        finish_soundcloud_page(&mut app);
        let loaded = page * 10;
        assert_eq!(app.view.selected, loaded);
        assert_eq!(app.soundcloud.selected, loaded);
        assert_eq!(app.view.rows[loaded].title, "Load more tracks…");
        assert_eq!(
            app.soundcloud.items[loaded - 10].title,
            format!("Track {}", loaded - 10)
        );
        assert_eq!(
            app.soundcloud.items[loaded - 1].title,
            format!("Track {}", loaded - 1)
        );
    }
    for (index, url) in requests.0.lock().unwrap().iter().enumerate() {
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(pairs["limit"], "10");
        assert_eq!(pairs["offset"], (index * 10).to_string());
        assert_eq!(pairs["q"], "fixture");
    }
    app.submit_soundcloud_search("next query".into());
    finish_soundcloud_page(&mut app);
    assert_eq!(app.soundcloud.items.len(), 100);
}

#[test]
fn soundcloud_load_more_does_not_steal_changed_selection_or_tab() {
    let (mut app, _) = paging_controller();
    app.set_soundcloud_search_page_capacity(10);
    app.submit_soundcloud_search("fixture".into());
    finish_soundcloud_page(&mut app);
    app.select_row(10);
    app.activate_soundcloud_selection();
    app.select_row(2);
    finish_soundcloud_page(&mut app);
    assert_eq!(app.view.selected, 2);
    app.select_row(20);
    app.activate_soundcloud_selection();
    app.show_screen(Screen::Search);
    let rows = app.view.rows.clone();
    let selected = app.view.selected;
    finish_soundcloud_page(&mut app);
    assert_eq!(app.view.rows, rows);
    assert_eq!(app.view.selected, selected);
    app.show_screen(Screen::SoundCloud);
    assert_eq!(
        app.view.selected, 20,
        "changing tabs cancels a pending page turn"
    );
    assert_eq!(app.soundcloud.items.len(), 30);
}

#[test]
fn soundcloud_final_page_and_result_cap_remove_the_continuation() {
    for initial in [3, 999] {
        let mut app = controller();
        complete(
            &mut app,
            0,
            (0..initial)
                .map(|index| track(&index.to_string()))
                .collect(),
            Some(2),
        );
        app.select_row(initial);
        app.soundcloud.page_turn = Some((0, initial));
        complete(
            &mut app,
            0,
            (initial..initial + 3)
                .map(|index| track(&index.to_string()))
                .collect(),
            (initial == 999).then_some(3),
        );
        let expected = (initial + 3).min(1_000);
        assert_eq!(app.soundcloud.items.len(), expected);
        assert_eq!(app.view.rows.len(), expected);
        assert_eq!(app.view.selected, expected - 1);
        assert_eq!(app.soundcloud.next_page, None);
    }
}

#[test]
fn soundcloud_partial_duplicate_page_keeps_the_provider_offset_stride() {
    let (mut app, requests) = paging_controller();
    app.soundcloud.page_limit = Some(10);
    app.soundcloud.submitted_query = "fixture".into();
    complete(
        &mut app,
        0,
        (0..10).map(|index| track(&index.to_string())).collect(),
        Some(2),
    );
    complete(
        &mut app,
        0,
        (8..18).map(|index| track(&index.to_string())).collect(),
        Some(3),
    );
    assert_eq!(app.soundcloud.items.len(), 18);
    app.select_row(18);
    app.activate_soundcloud_selection();
    finish_soundcloud_page(&mut app);
    let urls = requests.0.lock().unwrap();
    assert_eq!(urls.len(), 1);
    let query = urls[0].query_pairs().collect::<HashMap<_, _>>();
    assert_eq!(
        query["offset"], "20",
        "deduplicated row count must not replace provider offset"
    );
    assert_eq!(query["limit"], "10");
}

#[cfg(any(feature = "yandex-music", feature = "bbc-radio"))]
#[test]
fn soundcloud_intent_cancels_older_provider_resolution_before_metadata_returns() {
    let mut app = controller();
    install_playback_transport(&mut app);
    let item = queue_item_from_soundcloud(&track("one"));
    #[cfg(feature = "yandex-music")]
    {
        app.yandex_music_generation = 7;
        app.pending_yandex_music_playback = Some(PendingYandexMusicPlayback {
            generation: 7,
            track_id: "old-track".into(),
            item: item.clone(),
            queue_cursor_already_positioned: false,
            origin: None,
        });
    }
    #[cfg(feature = "bbc-radio")]
    {
        let mut old_item = item.clone();
        old_item.media.id = MediaId::new(SourceKind::Radio, "bbc_radio_three");
        app.bbc_playback_generation = 9;
        app.pending_bbc_playback = Some(PendingBbcPlayback {
            generation: 9,
            item: old_item,
            queue_cursor_already_positioned: false,
            origin: None,
        });
    }
    app.play_queue_item_with_origin(item, false, None);
    assert!(app.soundcloud_playback_pending());
    #[cfg(feature = "yandex-music")]
    {
        assert!(app.pending_yandex_music_playback.is_none());
        assert!(app.yandex_music_generation > 7);
    }
    #[cfg(feature = "bbc-radio")]
    {
        assert!(app.pending_bbc_playback.is_none());
        assert!(app.bbc_playback_generation > 9);
        let status = app.view.status_line.clone();
        app.handle_bbc_live(
            9,
            "bbc_radio_three".into(),
            Err("obsolete BBC resolution".into()),
        );
        assert_eq!(app.view.status_line, status);
        assert!(app.soundcloud_playback_pending());
        assert!(app.view.error_popup.is_none());
    }
}

/// Records actual backend loads without starting mpv, fetching media, or emitting sound.
struct RecordingPlayer(Arc<Mutex<Vec<PlaybackInput>>>);

/// Supplies action-resolved public metadata without a real instance or media fetch.
struct PlaybackMetadataTransport;

impl crate::providers::soundcloak::SoundcloakTransport for PlaybackMetadataTransport {
    fn fetch(&self, url: &url::Url, _: usize) -> Result<Vec<u8>, crate::providers::ProviderError> {
        assert_eq!(url.path(), "/_/api/v2/resolve");
        let canonical = url
            .query_pairs()
            .find(|(key, _)| key == "url")
            .unwrap()
            .1
            .into_owned();
        Ok(serde_json::to_vec(&serde_json::json!({
            "id": 1, "kind": "track", "title": "Track one", "user": {"username": "Fixture Artist"},
            "permalink_url": canonical, "duration": 120_000, "full_duration": 120_000,
            "policy": "ALLOW", "streamable": true,
            "media": {"transcodings": [{"snipped": false, "format": {"protocol": "hls", "mime_type": "audio/mpeg"}}]},
        })).unwrap())
    }
}

/// Installs the fixture on the currently configured instance, including replay after a change.
fn install_playback_transport(app: &mut AppController) {
    app.soundcloud.client = Some(
        SoundcloakClient::with_transport(
            app.config.providers.soundcloak_base_url.clone().unwrap(),
            Arc::new(PlaybackMetadataTransport),
        )
        .unwrap(),
    );
}

/// Waits for one bounded metadata response to reach the recording backend.
fn finish_playback(
    app: &mut AppController,
    played: &Arc<Mutex<Vec<PlaybackInput>>>,
    expected: usize,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while played.lock().unwrap().len() < expected {
        assert!(Instant::now() < deadline, "{}", app.view.status_line);
        app.poll_soundcloud_playback();
        thread::yield_now();
    }
}

impl PlaybackBackend for RecordingPlayer {
    fn play(&mut self, input: &PlaybackInput) -> PlaybackResult<()> {
        self.0.lock().unwrap().push(input.clone());
        Ok(())
    }

    fn command(&mut self, _command: PlayerCommand) -> PlaybackResult<()> {
        Ok(())
    }

    fn status(&mut self) -> PlaybackResult<PlaybackStatus> {
        Ok(PlaybackStatus::default())
    }

    fn shutdown(&mut self) -> PlaybackResult<()> {
        Ok(())
    }
}

#[test]
fn soundcloud_play_and_history_replay_bypass_ytdlp_and_keep_canonical_identity() {
    let mut controller = controller();
    install_playback_transport(&mut controller);
    let played = Arc::new(Mutex::new(Vec::new()));
    controller.player = Some(Box::new(RecordingPlayer(Arc::clone(&played))));
    complete(&mut controller, 0, vec![track("one"), track("two")], None);
    controller.activate_selection();
    finish_playback(&mut controller, &played, 1);
    assert_eq!(played.lock().unwrap().len(), 1);
    let item = controller.playback_queue.items.last().unwrap();
    assert_eq!(
        item.playback_location,
        "https://soundcloud.com/fixture-artist/one"
    );
    assert!(played.lock().unwrap()[0].bypass_ytdl);
    let entry = HistoryEntry {
        id: 1,
        media_id: item.media.id.clone(),
        title: item.media.title.clone(),
        replay_locator: Some(item.playback_location.clone()),
        started_at: 1,
        last_played_at: 1,
        position_seconds: 0,
        duration_seconds: Some(120),
        finished: false,
    };
    let target = history_replay_target(&entry).unwrap();
    let replay = queue_item_from_history(&entry, &target).unwrap();
    controller.config.providers.soundcloak_base_url =
        Some(url::Url::parse("https://another.example/").unwrap());
    install_playback_transport(&mut controller);
    controller.play_queue_item_with_origin(replay, false, None);
    finish_playback(&mut controller, &played, 2);
    let played = played.lock().unwrap();
    assert_eq!(played.len(), 2);
    assert!(
        played[1]
            .location
            .starts_with("https://another.example/_/api/hls/fixture-artist/one?")
    );
    assert!(played[1].bypass_ytdl);
}
