//! Offline controller coverage for Soundcloak navigation and canonical replay.

use super::*;

/// Builds public metadata without contacting any real instance.
fn track(slug: &str) -> SoundcloakTrack {
    SoundcloakTrack {
        id: slug.to_owned(),
        title: format!("Track {slug}"),
        artist: "Fixture Artist".to_owned(),
        webpage_url: url::Url::parse(&format!("https://soundcloud.com/fixture-artist/{slug}"))
            .unwrap(),
        artwork_url: None,
        duration_seconds: Some(120),
        description: Some("Fixture description".to_owned()),
        streamable: true,
    }
}

/// Uses in-memory persistence and an explicit test instance, with no live network.
fn controller() -> AppController {
    let mut config = Config::for_dir("/tmp/youta-soundcloud-controller-test");
    config.providers.soundcloak_base_url =
        Some(url::Url::parse("https://soundcloak.example/").unwrap());
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    controller.show_screen(Screen::SoundCloud);
    controller
}

/// Supplies a completion without creating a worker or fetching media.
fn complete(
    controller: &mut AppController,
    generation: u64,
    items: Vec<SoundcloakTrack>,
    next_page: Option<u32>,
) {
    controller.apply_soundcloud_page(
        SearchJob {
            generation,
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
    let input = controller.soundcloud_playback_input(&item).unwrap();
    assert!(input.bypass_ytdl);
    assert!(
        input
            .location
            .starts_with("https://soundcloak.example/_/api/hls/fixture-artist/one?")
    );
    assert!(input.location.contains("redirect_parts=false"));
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

/// Records actual backend loads without starting mpv, fetching media, or emitting sound.
struct RecordingPlayer(Arc<Mutex<Vec<PlaybackInput>>>);

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
    let played = Arc::new(Mutex::new(Vec::new()));
    controller.player = Some(Box::new(RecordingPlayer(Arc::clone(&played))));
    complete(&mut controller, 0, vec![track("one"), track("two")], None);
    controller.activate_selection();
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
    controller.play_queue_item_with_origin(replay, false, None);
    let played = played.lock().unwrap();
    assert_eq!(played.len(), 2);
    assert!(
        played[1]
            .location
            .starts_with("https://another.example/_/api/hls/fixture-artist/one?")
    );
    assert!(played[1].bypass_ytdl);
}

#[test]
fn soundcloud_old_public_share_links_use_the_proxy_without_accepting_private_or_foreign_urls() {
    let controller = controller();
    for url in [
        "https://www.soundcloud.com/fixture-artist/one/",
        "http://soundcloud.com/fixture-artist/one?utm_source=clipboard#t=30",
    ] {
        let item = queue_item_from_direct(&DirectSourceInput {
            source: SourceKind::SoundCloud,
            url: url::Url::parse(url).unwrap(),
        });
        let input = controller.soundcloud_playback_input(&item).unwrap();
        assert!(
            input
                .location
                .starts_with("https://soundcloak.example/_/api/hls/fixture-artist/one?")
        );
        assert!(!input.location.contains("utm_source"));
    }
    for url in [
        "https://soundcloud.com/fixture-artist/one?secret_token=s-private",
        "https://soundcloud.com/fixture-artist/one/s-private",
        "https://foreign.example/fixture-artist/one",
        "https://user:password@soundcloud.com/fixture-artist/one",
        "https://soundcloud.com/fixture-artist/sets",
    ] {
        let item = queue_item_from_direct(&DirectSourceInput {
            source: SourceKind::SoundCloud,
            url: url::Url::parse(url).unwrap(),
        });
        assert!(
            controller.soundcloud_playback_input(&item).is_err(),
            "{url}"
        );
    }
}
