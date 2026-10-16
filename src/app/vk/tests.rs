//! Offline controller coverage for VK identity, privacy and result ownership.

use super::*;
use std::sync::Mutex;

/// Constructs the controller without provider credentials or a live transport.
fn controller() -> AppController {
    let mut app = AppController::new(
        Config::for_dir("/tmp/youta-vk-controller-fixture"),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    app.show_screen(Screen::Vk);
    app
}

/// Keeps synthetic public metadata separate from private provider response fields.
fn track(id: u64) -> VkTrack {
    VkTrack::fixture(
        VkAudioId::new(7, id).unwrap(),
        "Fixture track",
        "Fixture artist",
    )
}

/// Entering an unconfigured tab must explain setup without inspecting any browser.
#[test]
fn vk_without_an_explicit_cookie_file_is_idle_and_explains_setup() {
    let mut app = controller();
    assert!(app.view.rows.is_empty());
    assert!(app.vk.worker.is_none());
    assert!(app.vk.pending_search.is_none());
    let details = app.view.details.as_ref().unwrap();
    assert!(
        details
            .description
            .contains(crate::config::VK_COOKIES_FILE_ENV)
    );
    assert!(details.description.contains("Netscape"));
    app.view.search_query = "fixture search".to_owned();
    app.submit_search();
    assert!(app.vk.worker.is_none());
    assert!(app.vk.pending_search.is_none());
    assert_eq!(search_route(Screen::Vk), SearchRoute::Vk);
}

/// Returning to a failed search must retain its explanation instead of reporting zero results.
#[test]
fn vk_search_failure_remains_visible_after_leaving_and_returning() {
    let mut app = controller();
    let path = PathBuf::from("/tmp/youta-vk-search-error-cookies");
    app.config.providers.vk_cookies_file = Some(path.clone());
    app.vk.cookie_file = Some(path);
    app.apply_vk_page(0, false, Err("Fixture authentication failure".to_owned()));
    assert_eq!(app.view.status_line, "VK: Fixture authentication failure");
    let details = app
        .view
        .details
        .as_ref()
        .expect("visible VK failure details");
    assert!(
        details
            .description
            .contains("Fixture authentication failure")
    );
    assert!(details.media_id.is_none());
    assert!(details.webpage_url.is_none());
    assert!(details.links.is_empty());
    app.show_screen(Screen::Local);
    app.show_screen(Screen::Vk);
    assert_eq!(app.view.status_line, "VK: Fixture authentication failure");
    app.show_screen(Screen::Local);
    let hidden_status = app.view.status_line.clone();
    let hidden_details = app.view.details.clone();
    app.apply_vk_page(0, false, Err("Fixture request failure".to_owned()));
    assert_eq!(app.view.status_line, hidden_status);
    assert_eq!(app.view.details, hidden_details);
    app.show_screen(Screen::Vk);
    assert_eq!(app.view.status_line, "VK: Fixture request failure");
    assert!(
        app.view
            .details
            .as_ref()
            .unwrap()
            .description
            .contains("Fixture request failure")
    );
    assert!(app.view.rows.is_empty());
    assert!(app.vk.worker.is_none());
}

/// Success, new navigation and replaced credentials cannot revive an older search failure.
#[test]
fn vk_search_failure_is_owned_by_current_navigation_and_import() {
    let mut app = controller();
    let path = PathBuf::from("/tmp/youta-vk-search-error-cookies");
    app.config.providers.vk_cookies_file = Some(path.clone());
    app.vk.cookie_file = Some(path);
    app.vk.generation = 2;
    app.apply_vk_page(2, false, Err("Current request failed".to_owned()));
    app.apply_vk_page(1, false, Err("Obsolete request failed".to_owned()));
    app.populate_vk();
    assert_eq!(app.view.status_line, "VK: Current request failed");
    app.apply_vk_page(
        2,
        false,
        Ok(VkPage {
            tracks: vec![track(1)],
            next: None,
        }),
    );
    app.show_screen(Screen::Local);
    app.show_screen(Screen::Vk);
    assert!(app.view.status_line.starts_with("1 VK tracks"));
    assert_eq!(
        app.view.details.as_ref().unwrap().media_id,
        Some(media_id(track(1).id))
    );
    app.vk.items.clear();
    app.apply_vk_page(2, false, Err("Old navigation failed".to_owned()));
    app.vk.generation += 1;
    app.populate_vk();
    assert!(!app.view.status_line.contains("failed"));
    assert!(app.view.details.is_none());
    app.apply_vk_page(3, false, Err("Old import failed".to_owned()));
    app.config.providers.vk_cookies_file = Some(PathBuf::from("/tmp/youta-vk-new-search-cookies"));
    app.populate_vk();
    assert!(!app.view.status_line.contains("failed"));
    assert!(app.view.details.is_none());
    app.refresh_vk_cookie_owner();
    app.populate_vk();
    assert!(app.view.status_line.starts_with("0 VK tracks"));
    assert!(app.view.details.is_none());
}

/// Saved playlists and queues may contain public pages, never transient media locators.
#[test]
fn vk_queue_metadata_contains_only_canonical_audio_identity() {
    let track = track(11);
    let item = queue_item(&track);
    assert_eq!(item.media.id, MediaId::new(SourceKind::Vk, "7_11"));
    assert_eq!(item.media.webpage_url, track.id.webpage_url());
    assert_eq!(item.playback_location, track.id.webpage_url().to_string());
    assert_eq!(audio_identity(&item), Some(track.id));
    let encoded = serde_json::to_string(&item).unwrap();
    assert!(!encoded.contains("vkuseraudio"));
    assert!(!encoded.contains("reload"));
    let mut video = item.clone();
    video.media.kind = MediaKind::Video;
    assert!(audio_identity(&video).is_none());
    let mut mismatch = item;
    mismatch.media.webpage_url = VkAudioId::new(8, 11).unwrap().webpage_url();
    assert!(audio_identity(&mismatch).is_none());
}

/// An obsolete completion cannot replace current results or another source's panel.
#[test]
fn vk_catalogue_results_are_generation_owned_and_do_not_paint_hidden_tabs() {
    let mut app = controller();
    app.vk.generation = 2;
    app.apply_vk_page(
        1,
        false,
        Ok(VkPage {
            tracks: vec![track(1)],
            next: None,
        }),
    );
    assert!(app.vk.items.is_empty());
    app.show_screen(Screen::Local);
    let rows = app.view.rows.clone();
    let details = app.view.details.clone();
    app.apply_vk_page(
        2,
        false,
        Ok(VkPage {
            tracks: vec![track(2)],
            next: None,
        }),
    );
    assert_eq!(app.vk.items.len(), 1);
    assert_eq!(app.view.rows, rows);
    assert_eq!(app.view.details, details);
    app.show_screen(Screen::Vk);
    assert_eq!(
        app.view.rows[0].media_id,
        Some(MediaId::new(SourceKind::Vk, "7_2"))
    );
}

/// Duplicate continuations never grow the list or retain a request-loop sentinel.
#[test]
fn vk_page_merge_deduplicates_and_details_use_the_selected_public_page() {
    let mut app = controller();
    app.apply_vk_page(
        0,
        false,
        Ok(VkPage {
            tracks: vec![track(1), track(2)],
            next: None,
        }),
    );
    app.apply_vk_page(
        0,
        true,
        Ok(VkPage {
            tracks: vec![track(2), track(3)],
            next: None,
        }),
    );
    assert_eq!(app.vk.items.len(), 3);
    app.select_row(2);
    let selected = &app.vk.items[2];
    let details = app.view.details.as_ref().unwrap();
    assert_eq!(details.media_id, Some(MediaId::new(SourceKind::Vk, "7_3")));
    assert!(
        details
            .links
            .iter()
            .any(|link| link.url == selected.id.webpage_url().as_str())
    );
    assert_eq!(
        app.selected_queue_item().unwrap().media.id,
        details.media_id.clone().unwrap()
    );
}

/// Album text follows the selected track without changing its public metadata or links.
#[test]
fn vk_selected_track_album_details_do_not_leak_into_tracks_without_albums() {
    let mut app = controller();
    let mut album_track = VkTrack::fixture_with_album(
        VkAudioId::new(7, 2).unwrap(),
        "Fixture track",
        "Fixture artist",
    );
    album_track.album_title = Some("Fixture album".to_owned());
    album_track.artwork_url =
        Some(url::Url::parse("https://sun1.userapi.com/fixture.jpg").unwrap());
    album_track.artwork_dimensions = Some((300, 300));
    let mut blank_album_track = track(3);
    blank_album_track.album_title = Some(" \t\n ".to_owned());
    app.apply_vk_page(
        0,
        false,
        Ok(VkPage {
            tracks: vec![track(1), album_track.clone(), blank_album_track],
            next: None,
        }),
    );
    assert!(app.view.details.as_ref().unwrap().description.is_empty());

    app.select_row(1);
    let details = app.view.details.as_ref().unwrap();
    assert!(details.description.is_empty());
    assert_eq!(details.media_id, Some(MediaId::new(SourceKind::Vk, "7_2")));
    assert_eq!(details.title, album_track.title);
    assert_eq!(details.channel_name, album_track.artist);
    assert_eq!(details.source, "VK");
    assert_eq!(details.length, "2:00");
    assert_eq!(details.thumbnail_url, album_track.artwork_url);
    assert_eq!(details.thumbnail_dimensions, Some((300, 300)));
    assert_eq!(details.webpage_url, Some(album_track.id.webpage_url()));
    assert_eq!(details.links.len(), 2);
    assert!(details.links[0].label.is_empty());
    assert_eq!(details.links[0].url, album_track.id.webpage_url().as_str());

    app.select_row(0);
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
    assert_eq!(
        app.view.details.as_ref().unwrap().thumbnail_dimensions,
        None
    );
    app.select_row(1);
    assert_eq!(
        app.view.details.as_ref().unwrap().links[1].label,
        "Fixture album"
    );
    app.select_row(2);
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
    assert!(app.vk.worker.is_none());
    assert!(app.vk.pending_search.is_none());
    assert!(app.vk.pending_playback.is_none());
}

/// VK can ignore its requested count; advancing one screen must not skip the extra rows.
#[test]
fn vk_oversized_continuation_selects_only_the_first_new_viewport() {
    let mut app = controller();
    app.update_vk_search_page_capacities(crate::view::SearchPageCapacities::fixed(3));
    app.apply_vk_page(
        0,
        false,
        Ok(VkPage {
            tracks: (1..=3).map(track).collect(),
            next: None,
        }),
    );
    app.vk.selected = 3;
    app.view.selected = 3;
    app.vk.page_turn = Some(3);
    app.apply_vk_page(
        0,
        true,
        Ok(VkPage {
            tracks: (4..=11).map(track).collect(),
            next: None,
        }),
    );
    assert_eq!(app.vk.items.len(), 11, "all fetched rows remain reachable");
    assert_eq!(
        app.view.selected, 5,
        "only rows 3, 4 and 5 advance into view"
    );
}

/// Canceling a resolver revokes its completion without stopping accepted audio.
#[test]
fn vk_shutdown_and_playback_cancellation_revoke_pending_owners() {
    let mut app = controller();
    let item = queue_item(&track(5));
    app.vk.pending_playback = Some(PlaybackJob {
        generation: 0,
        item,
        queued: false,
        origin: None,
    });
    assert!(app.vk_playback_pending());
    app.cancel_vk_playback();
    assert!(!app.vk_playback_pending());
    let generation = app.vk.playback_generation;
    app.shutdown_vk();
    assert!(app.vk.playback_generation > generation);
    assert!(app.vk.pending_search.is_none());
    assert!(!app.vk.restore);
}

/// One occupied lane lets repeated searches replace only the single queued intent.
#[test]
fn vk_searches_coalesce_behind_an_owned_worker_without_starting_parallel_http() {
    let mut app = controller();
    let path = PathBuf::from("/tmp/youta-vk-explicit-fixture-cookies");
    app.config.providers.vk_cookies_file = Some(path.clone());
    app.vk.cookie_file = Some(path.clone());
    app.apply_vk_page(0, false, Err("Previous request failed".to_owned()));
    let (_sender, response) = bounded(1);
    app.vk.worker = Some(Worker {
        job: Job::Search(SearchJob {
            generation: 0,
            query: VkQuery::Search("old".to_owned()),
            kind: VkSearchKind::Tracks,
            cursor: None,
        }),
        cookie_file: path,
        cookie_generation: 0,
        response,
        thread: thread::spawn(|| {}),
    });
    for query in ["first", "second", "newest"] {
        app.view.search_query = query.to_owned();
        app.submit_search();
    }
    assert!(
        matches!(app.vk.pending_search.as_ref().unwrap().query, VkQuery::Search(ref query) if query == "newest")
    );
    assert!(
        matches!(&app.vk.worker.as_ref().unwrap().job, Job::Search(job) if job.generation == 0)
    );
    assert_eq!(app.vk.generation, 3);
    assert_eq!(app.view.search_activity, Some(SearchActivity::Vk));
    app.populate_vk();
    assert!(!app.view.status_line.contains("failed"));
    assert!(
        app.view.details.is_none(),
        "a new search clears the prior diagnostic panel"
    );
    app.show_screen(Screen::Local);
    assert_eq!(app.vk.query, "newest");
    assert_ne!(app.view.search_activity, Some(SearchActivity::Vk));
}

/// Bare numeric searches retain song-search intent behind a held provider worker.
#[test]
fn vk_numeric_music_queries_queue_search_instead_of_profile_browsing() {
    let mut app = controller();
    let path = PathBuf::from("/tmp/youta-vk-numeric-query-fixture-cookies");
    app.config.providers.vk_cookies_file = Some(path.clone());
    app.vk.cookie_file = Some(path.clone());
    let (_sender, response) = bounded(1);
    app.vk.worker = Some(Worker {
        job: Job::Search(SearchJob {
            generation: 0,
            query: VkQuery::Search("held fixture".to_owned()),
            kind: VkSearchKind::Tracks,
            cursor: None,
        }),
        cookie_file: path,
        cookie_generation: 0,
        response,
        thread: thread::spawn(|| {}),
    });
    for query in ["2032", "0", "00123", "9223372036854775808"] {
        app.view.search_query = query.to_owned();
        app.submit_search();
        assert_eq!(
            app.vk.pending_search.as_ref().unwrap().query,
            VkQuery::Search(query.to_owned())
        );
    }
    for query in ["id2032", "profile:2032", "@id2032", "https://vk.ru/id2032"] {
        app.view.search_query = query.to_owned();
        app.submit_search();
        assert!(matches!(
            &app.vk.pending_search.as_ref().unwrap().query,
            VkQuery::Profile(profile) if profile.user_id() == Some(2032)
        ));
    }
    assert!(matches!(
        &app.vk.worker.as_ref().unwrap().job,
        Job::Search(job) if job.generation == 0
    ));
}

/// A title match is not sufficient to replace a saved track with a different recording.
#[test]
fn vk_bounded_replay_requires_exact_identity_even_when_titles_are_equal() {
    let requested = VkAudioId::new(7, 2).unwrap();
    assert!(
        exact_track(
            VkPage {
                tracks: vec![track(1)],
                next: None
            },
            requested
        )
        .is_err()
    );
    let matched = exact_track(
        VkPage {
            tracks: vec![track(1), track(2)],
            next: None,
        },
        requested,
    )
    .unwrap();
    assert_eq!(matched.id, requested);
}

/// Captures player input without starting a subprocess or contacting a media server.
struct Player(Arc<Mutex<Vec<PlaybackInput>>>);

impl PlaybackBackend for Player {
    fn play(&mut self, input: &PlaybackInput) -> crate::playback::Result<()> {
        self.0.lock().unwrap().push(input.clone());
        Ok(())
    }

    fn command(&mut self, _: PlayerCommand) -> crate::playback::Result<()> {
        Ok(())
    }
    fn status(&mut self) -> crate::playback::Result<PlaybackStatus> {
        Ok(PlaybackStatus::default())
    }
    fn shutdown(&mut self) -> crate::playback::Result<()> {
        Ok(())
    }
}

/// A signed audio URL reaches only the backend; queue, navigation and IPC remain canonical.
#[test]
fn vk_resolved_playback_keeps_private_media_urls_out_of_queue_and_view() {
    let mut app = controller();
    let played = Arc::new(Mutex::new(Vec::new()));
    app.player = Some(Box::new(Player(Arc::clone(&played))));
    let track = track(11);
    let canonical = track.id.webpage_url().to_string();
    let item = queue_item(&track);
    let location = crate::providers::vk::decode_audio_url(
        "https://fixture.vkuseraudio.net/audio.m3u8?extra=private-replay-canary",
        1,
    )
    .unwrap();
    app.apply_vk_playback(
        PlaybackJob {
            generation: 0,
            item,
            queued: false,
            origin: None,
        },
        Ok(Reply::Playback(Box::new(track), location)),
    );
    assert_eq!(played.lock().unwrap().len(), 1);
    assert!(played.lock().unwrap()[0].bypass_ytdl);
    assert!(
        played.lock().unwrap()[0]
            .location
            .contains("private-replay-canary")
    );
    assert_eq!(
        app.playback_queue.current().unwrap().playback_location,
        canonical
    );
    assert_eq!(app.view.playing_screen, Some(Screen::Vk));
    assert!(
        !serde_json::to_string(&app.view)
            .unwrap()
            .contains("private-replay-canary")
    );
    assert!(
        !serde_json::to_string(&app.playback_queue)
            .unwrap()
            .contains("private-replay-canary")
    );
    assert!(app.reveal_retained_now_playing(&app.playback_queue.current().unwrap().clone()));
    assert_eq!(app.view.screen, Screen::Vk);
    assert_eq!(app.view.rows[0].media_id, app.current_media);
}

/// A canceled worker cannot replace accepted playback when its result finally arrives.
#[test]
fn vk_obsolete_playback_completion_never_loads_the_player() {
    let mut app = controller();
    let played = Arc::new(Mutex::new(Vec::new()));
    app.player = Some(Box::new(Player(Arc::clone(&played))));
    let path = PathBuf::from("/tmp/youta-vk-explicit-fixture-cookies");
    app.config.providers.vk_cookies_file = Some(path.clone());
    app.vk.cookie_file = Some(path.clone());
    let track = track(11);
    let job = PlaybackJob {
        generation: 0,
        item: queue_item(&track),
        queued: false,
        origin: None,
    };
    let location = crate::providers::vk::decode_audio_url(
        "https://fixture.vkuseraudio.net/audio.m3u8?extra=obsolete",
        1,
    )
    .unwrap();
    let (sender, response) = bounded(1);
    assert!(
        sender
            .send(Completion {
                client: None,
                result: Ok(Reply::Playback(Box::new(track), location))
            })
            .is_ok()
    );
    app.vk.worker = Some(Worker {
        job: Job::Playback(Box::new(job)),
        cookie_file: path,
        cookie_generation: 0,
        response,
        thread: thread::spawn(|| {}),
    });
    app.cancel_vk_playback();
    app.poll_vk_worker();
    assert!(played.lock().unwrap().is_empty());
    assert!(app.current_media.is_none());
    assert!(app.vk.worker.is_none());
}

/// Repeated history replays must not turn private runtime metadata into an unbounded cache.
#[test]
fn vk_playback_metadata_cache_remains_bounded_without_catalogue_searches() {
    let mut app = controller();
    let played = Arc::new(Mutex::new(Vec::new()));
    app.player = Some(Box::new(Player(played)));
    app.vk.retained = (1..=2_000)
        .map(|id| {
            let track = track(id);
            (track.id, track)
        })
        .collect();
    let track = track(2_001);
    let job = PlaybackJob {
        generation: 0,
        item: queue_item(&track),
        queued: false,
        origin: None,
    };
    let location = crate::providers::vk::decode_audio_url(
        "https://fixture.vkuseraudio.net/audio.m3u8?extra=bounded",
        1,
    )
    .unwrap();
    app.apply_vk_playback(job, Ok(Reply::Playback(Box::new(track), location)));
    assert!(app.vk.retained.len() <= 2_000);
    assert!(
        app.vk
            .retained
            .contains_key(&VkAudioId::new(7, 2_001).unwrap())
    );
}

/// A malformed persisted native identity must not be handed to the generic extractor.
#[test]
fn vk_corrupt_audio_identity_never_falls_back_to_generic_playback() {
    let mut app = controller();
    let played = Arc::new(Mutex::new(Vec::new()));
    app.player = Some(Box::new(Player(Arc::clone(&played))));
    let mut item = queue_item(&track(1));
    item.media.webpage_url = track(2).id.webpage_url();
    item.playback_location = item.media.webpage_url.to_string();
    app.play_queue_item(item, false);
    assert!(played.lock().unwrap().is_empty());
    assert!(app.view.status_line.contains("identity"));
    assert!(app.vk.worker.is_none());
}

/// Changing the import away and back cannot revive the first import's stale client.
#[test]
fn vk_cookie_path_round_trip_does_not_adopt_an_obsolete_client() {
    let mut app = controller();
    let original = PathBuf::from("/tmp/youta-vk-cookie-owner-a");
    app.config.providers.vk_cookies_file = Some(original.clone());
    app.vk.cookie_file = Some(original.clone());
    let cookies =
        crate::providers::vk::VkCookieJar::parse(b"vk.ru\tFALSE\t/\tTRUE\t0\tsid\tfixture\n")
            .unwrap();
    let client = VkClient::from_cookies(cookies);
    let (sender, response) = bounded(1);
    assert!(
        sender
            .send(Completion {
                client: Some(client),
                result: Ok(Reply::Page(VkPage {
                    tracks: Vec::new(),
                    next: None
                }))
            })
            .is_ok()
    );
    app.vk.worker = Some(Worker {
        job: Job::Search(SearchJob {
            generation: 0,
            query: VkQuery::Search("obsolete".to_owned()),
            kind: VkSearchKind::Tracks,
            cursor: None,
        }),
        cookie_file: original.clone(),
        cookie_generation: 0,
        response,
        thread: thread::spawn(|| {}),
    });
    app.config.providers.vk_cookies_file = Some(PathBuf::from("/tmp/youta-vk-cookie-owner-b"));
    app.refresh_vk_cookie_owner();
    app.config.providers.vk_cookies_file = Some(original);
    app.refresh_vk_cookie_owner();
    app.poll_vk_worker();
    assert!(
        app.vk.client.is_none(),
        "a credential replacement must revoke old authenticated clients"
    );
}
