//! Offline owner, privacy, paging, and playback tests for Funkwhale integration.

use super::*;
use crate::providers::{SearchPage, Thumbnail};

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

/// Captures exactly the selected pod and source-local request owner.
fn search_job(app: &AppController, generation: u64) -> Job {
    Job::Search {
        generation,
        pod: app.config.providers.funkwhale_instance_url.clone().unwrap(),
        query: "fixture".to_owned(),
        page: 1,
        size: app.funkwhale.page_size,
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
    assert_eq!(app.funkwhale.capacity, 100);
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
        Ok(Reply::Details(details(&track("pod.example", 2)))),
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
