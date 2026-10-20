//! Offline album-search coverage for worker ownership, navigation, and persistence.

use super::*;

/// Holds the authenticated worker lane so every submitted request remains inspectable.
fn controller() -> (AppController, Sender<Completion>) {
    let mut app = AppController::new(
        Config::for_dir("/tmp/youta-vk-album-search-fixture"),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    app.show_screen(Screen::Vk);
    let path = PathBuf::from("/tmp/youta-vk-album-search-cookies");
    app.config.providers.vk_cookies_file = Some(path.clone());
    app.vk.cookie_file = Some(path.clone());
    let (sender, response) = bounded(1);
    app.vk.worker = Some(Worker {
        job: Job::Search(SearchJob {
            generation: 0,
            query: VkQuery::Search("held fixture".into()),
            kind: VkSearchKind::Tracks,
            cursor: None,
        }),
        cookie_file: path,
        cookie_generation: 0,
        response,
        thread: thread::spawn(|| {}),
    });
    (app, sender)
}

/// Supplies canonical album metadata with provider-private request ownership.
fn album(id: u64) -> VkAlbum {
    VkAlbum::fixture(7, id, &format!("Album {id}"), "Fixture artist")
}

/// Album searches use the same bounded lane while direct audio/profile input keeps its meaning.
#[test]
fn vk_album_mode_dispatches_text_and_preserves_explicit_track_queries() {
    let (mut app, _sender) = controller();
    app.view.search_query = "2032".into();
    app.dispatch(UiAction::CycleVkSearchKind);
    assert_eq!(app.view.vk_search_kind, VkSearchKind::Albums);
    let job = app.vk.pending_search.as_ref().unwrap();
    assert_eq!(job.kind, VkSearchKind::Albums);
    assert_eq!(job.query, VkQuery::Search("2032".into()));
    let generation = app.vk.generation;
    app.submit_vk_search("another album".into());
    assert!(app.vk.generation > generation);
    assert_eq!(
        app.vk.pending_search.as_ref().unwrap().kind,
        VkSearchKind::Albums
    );
    for query in ["profile:2032", "https://vk.ru/audio7_2"] {
        app.view.vk_search_kind = VkSearchKind::Albums;
        app.submit_vk_search(query.into());
        assert_eq!(app.view.vk_search_kind, VkSearchKind::Tracks);
        assert_eq!(
            app.vk.pending_search.as_ref().unwrap().kind,
            VkSearchKind::Tracks
        );
    }
    assert!(
        matches!(&app.vk.worker.as_ref().unwrap().job, Job::Search(job) if job.generation == 0)
    );
}

/// An album opens a track page and Back restores the complete album search without a request.
#[test]
fn vk_album_search_opens_selected_album_and_back_rejects_late_tracks() {
    let (mut app, _sender) = controller();
    app.view.vk_search_kind = VkSearchKind::Albums;
    app.vk.query = "fixture albums".into();
    app.view.search_query = app.vk.query.clone();
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1), album(2)],
            next: None,
        }),
    );
    app.select_row(1);
    assert!(app.view.rows.iter().all(|row| row.media_id.is_none()));
    assert!(app.selected_vk_queue_item().is_err());
    assert_eq!(app.current_url(), Some(album(2).webpage_url().to_string()));
    assert_eq!(app.view.details.as_ref().unwrap().title, "Album 2");
    app.activate_vk_selection();
    assert_eq!(app.vk.results_kind, VkSearchKind::Tracks);
    assert_eq!(app.view.vk_search_kind, VkSearchKind::Albums);
    assert!(
        matches!(&app.vk.pending_browse.as_ref().unwrap().source, BrowseSource::Album(selected) if selected.webpage_url() == album(2).webpage_url())
    );
    let generation = app.vk.generation;
    assert!(app.back_vk_catalogue());
    assert_eq!(app.vk.results_kind, VkSearchKind::Albums);
    assert_eq!(app.view.selected, 1);
    assert_eq!(app.view.search_query, "fixture albums");
    assert_ne!(app.view.search_activity, Some(SearchActivity::Vk));
    app.apply_vk_page(
        generation,
        false,
        Ok(VkPage {
            tracks: Vec::new(),
            next: None,
        }),
    );
    assert_eq!(app.vk.albums.len(), 2);
    assert_eq!(app.view.rows[1].title, "Album 2");
}

/// Album result pages survive reversible now-playing navigation and never become queue entries.
#[test]
fn vk_album_results_survive_now_playing_snapshot_swaps() {
    let (mut app, _sender) = controller();
    let track = VkTrack::fixture(VkAudioId::new(7, 9).unwrap(), "Track", "Artist");
    let item = queue_item(&track);
    app.retain_vk_track(track);
    let mut snapshot = app.vk_navigation_snapshot(&item).unwrap();
    app.view.vk_search_kind = VkSearchKind::Albums;
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1)],
            next: None,
        }),
    );
    app.swap_vk_navigation(&mut snapshot);
    app.populate_vk();
    assert_eq!(app.view.vk_search_kind, VkSearchKind::Tracks);
    assert_eq!(
        app.selected_vk_queue_item().unwrap().media.id,
        item.media.id
    );
    app.swap_vk_navigation(&mut snapshot);
    app.populate_vk();
    assert_eq!(app.view.vk_search_kind, VkSearchKind::Albums);
    assert_eq!(app.vk.results_kind, VkSearchKind::Albums);
    assert_eq!(app.vk.albums.len(), 1);
    assert!(app.selected_vk_queue_item().is_err());
}

/// A restored album row survives the empty startup projection until its actual page arrives.
#[test]
fn vk_restored_album_selection_survives_initial_empty_view() {
    let (mut app, _sender) = controller();
    let saved = SessionState {
        vk_search_kind: VkSearchKind::Albums,
        vk_search_text: "saved albums".into(),
        vk_selected_row: Some(1),
        ..SessionState::default()
    };
    let worker = app.vk.worker.take();
    app.vk = VkState::restored(&saved);
    app.vk.worker = worker;
    app.vk.cookie_file = app.config.providers.vk_cookies_file.clone();
    app.view.vk_search_kind = saved.vk_search_kind;
    app.view.search_query = saved.vk_search_text.clone();
    app.populate_vk();
    assert_eq!(app.view.selected, 0);
    app.poll_vk_worker();
    assert_eq!(
        app.vk.pending_search.as_ref().unwrap().kind,
        VkSearchKind::Albums
    );
    app.populate_vk();
    app.apply_vk_album_page(
        app.vk.generation,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1), album(2)],
            next: None,
        }),
    );
    assert_eq!(app.view.selected, 1);
    assert_eq!(app.view.details.as_ref().unwrap().title, "Album 2");
}

/// Album content is accepted only through the matching live worker generation.
#[test]
fn vk_album_worker_discards_an_obsolete_track_reply() {
    let (mut app, sender) = controller();
    app.view.search_query = "fixture albums".into();
    app.dispatch(UiAction::CycleVkSearchKind);
    app.apply_vk_album_page(
        app.vk.generation,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1)],
            next: None,
        }),
    );
    app.vk.pending_search = None;
    sender
        .send(Completion {
            client: None,
            result: Ok(Reply::Page(VkPage {
                tracks: vec![VkTrack::fixture(
                    VkAudioId::new(7, 1).unwrap(),
                    "Obsolete track",
                    "Artist",
                )],
                next: None,
            })),
        })
        .unwrap();
    app.poll_vk_worker();
    assert_eq!(app.vk.results_kind, VkSearchKind::Albums);
    assert_eq!(app.view.rows[0].title, "Album 1");
    assert!(app.vk.items.is_empty());
    assert!(app.vk.worker.is_none());
}

/// Oversized album continuations advance one viewport and empty results stay non-playable.
#[test]
fn vk_album_page_turn_retains_extra_results_and_handles_empty_pages() {
    let (mut app, _sender) = controller();
    app.view.vk_search_kind = VkSearchKind::Albums;
    app.update_vk_search_page_capacities(crate::view::SearchPageCapacities::fixed(3));
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: (1..=3).map(album).collect(),
            next: None,
        }),
    );
    app.vk.selected = 3;
    app.view.selected = 3;
    app.vk.page_turn = Some(3);
    app.apply_vk_album_page(
        0,
        true,
        Ok(VkAlbumPage {
            albums: (4..=11).map(album).collect(),
            next: None,
        }),
    );
    assert_eq!(app.vk.albums.len(), 11);
    assert_eq!(app.view.selected, 5);
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: Vec::new(),
            next: None,
        }),
    );
    assert!(app.view.rows.is_empty());
    assert!(app.view.details.is_none());
    assert!(app.selected_vk_queue_item().is_err());
    app.activate_vk_selection();
    assert!(app.vk.pending_browse.is_none());
    assert!(app.vk.pending_playback.is_none());
}

/// A failed continuation retains its exact cursor for retry, while duplicates retire it.
#[test]
fn vk_album_continuation_retries_without_queuing_or_repeating_duplicates() {
    let (mut app, _sender) = controller();
    app.view.vk_search_kind = VkSearchKind::Albums;
    app.vk.worker.as_mut().unwrap().job = Job::Album(AlbumJob {
        generation: 0,
        track: Box::new(VkTrack::fixture(
            VkAudioId::new(7, 1).unwrap(),
            "Track",
            "Artist",
        )),
    });
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1), album(2)],
            next: Some(VkPageCursor::fixture_album_search()),
        }),
    );
    app.select_row(2);
    assert_eq!(app.view.rows[2].title, "Load more albums...");
    assert!(app.selected_vk_queue_item().is_err());
    assert!(app.current_url().is_none());
    app.activate_vk_selection();
    let pending = app.vk.pending_search.take().unwrap();
    assert_eq!(pending.kind, VkSearchKind::Albums);
    assert!(pending.cursor.is_some());
    app.apply_vk_album_page(0, true, Err("Continuation failure".into()));
    assert_eq!(app.vk.albums.len(), 2);
    assert!(app.vk.next.is_some());
    assert!(
        app.view
            .details
            .as_ref()
            .unwrap()
            .description
            .contains("Continuation failure")
    );
    app.activate_vk_selection();
    assert!(app.vk.pending_search.take().unwrap().cursor.is_some());
    app.apply_vk_album_page(
        0,
        true,
        Ok(VkAlbumPage {
            albums: vec![album(2)],
            next: Some(VkPageCursor::fixture_album_search()),
        }),
    );
    assert_eq!(app.vk.albums.len(), 2);
    assert!(app.vk.next.is_none());
    assert_eq!(app.view.rows.len(), 2);
    assert_eq!(app.view.selected, 1);
    assert!(app.vk.pending_playback.is_none());
}

/// Album result metadata cannot enable playback-only publishing or fingerprint controls.
#[test]
fn vk_album_result_selection_disables_track_actions() {
    let (mut app, _sender) = controller();
    app.view.vk_search_kind = VkSearchKind::Albums;
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1)],
            next: None,
        }),
    );
    app.refresh_selected_playlist_state();
    assert!(app.selected_queue_item().is_err());
    #[cfg(feature = "commons-upload")]
    assert!(!app.view.commons_upload_available);
    #[cfg(feature = "archive-upload")]
    assert!(!app.view.archive_upload_available);
    #[cfg(feature = "acoustid")]
    {
        app.fingerprint_selected_vk_audio();
        assert!(!app.vk_fingerprint_pending());
    }
    assert!(
        app.vk_autoplay_origin(&media_id(VkAudioId::new(7, 1).unwrap()))
            .is_none()
    );
}

/// Failure, success, deduplication, and hidden-tab publication mirror track search behavior.
#[test]
fn vk_album_results_keep_failures_owned_and_deduplicate_continuations() {
    let (mut app, _sender) = controller();
    app.view.vk_search_kind = VkSearchKind::Albums;
    app.vk.results_kind = VkSearchKind::Albums;
    app.apply_vk_album_page(0, false, Err("Fixture failure".into()));
    assert!(
        app.view
            .details
            .as_ref()
            .unwrap()
            .description
            .contains("Fixture failure")
    );
    app.show_screen(Screen::Local);
    let rows = app.view.rows.clone();
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1), album(2)],
            next: None,
        }),
    );
    assert_eq!(app.view.rows, rows);
    app.apply_vk_album_page(
        0,
        true,
        Ok(VkAlbumPage {
            albums: vec![album(2), album(3)],
            next: None,
        }),
    );
    assert_eq!(app.vk.albums.len(), 3);
    app.show_screen(Screen::Vk);
    assert!(app.view.status_line.starts_with("3 VK albums"));
    app.vk.generation += 1;
    app.apply_vk_album_page(0, false, Err("Stale failure".into()));
    assert!(!app.view.status_line.contains("Stale failure"));
    app.config.providers.vk_cookies_file = Some(PathBuf::from("/tmp/other-vk-album-session"));
    app.refresh_vk_cookie_owner();
    assert!(app.vk.albums.is_empty());
}

/// Restart restoration retains the album search's row rather than a child track position.
#[test]
fn vk_album_session_saves_parent_search_mode_and_selection() {
    let (mut app, _sender) = controller();
    app.view.vk_search_kind = VkSearchKind::Albums;
    app.vk.query = "fixture albums".into();
    app.view.search_query = app.vk.query.clone();
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1), album(2)],
            next: None,
        }),
    );
    app.select_row(1);
    app.activate_vk_selection();
    assert!(app.save_session());
    let saved = app.store.session().unwrap().unwrap();
    assert_eq!(saved.vk_search_kind, VkSearchKind::Albums);
    assert_eq!(saved.vk_selected_row, Some(1));
    assert_eq!(saved.vk_search_text, "fixture albums");
    let restored = VkState::restored(&saved);
    assert_eq!(restored.selected, 1);
    assert!(restored.restore);
    assert!(!serde_json::to_string(&saved).unwrap().contains("hash"));
    app.config.persistence.save_playback_history = false;
    assert!(app.save_session());
    assert_eq!(
        app.store.session().unwrap(),
        Some(saved),
        "disabling saving preserves the previous session"
    );
}

/// Bounded nested browsing preserves the original album search and its persisted row.
#[test]
fn vk_deep_catalogue_navigation_retains_root_album_search_selection() {
    let (mut app, _sender) = controller();
    app.view.vk_search_kind = VkSearchKind::Albums;
    app.vk.query = "root albums".into();
    app.view.search_query = app.vk.query.clone();
    app.apply_vk_album_page(
        0,
        false,
        Ok(VkAlbumPage {
            albums: vec![album(1), album(2)],
            next: None,
        }),
    );
    app.select_row(1);
    app.activate_vk_selection();
    for depth in 0..10 {
        let tracks = (1..=3)
            .map(|id| {
                let id = VkAudioId::new(7, id).unwrap();
                if depth % 2 == 0 {
                    VkTrack::fixture_with_album(id, "Nested track", "Artist")
                } else {
                    VkTrack::fixture_with_artist(id, "Nested track", "Artist")
                }
            })
            .collect();
        app.apply_vk_page(app.vk.generation, false, Ok(VkPage { tracks, next: None }));
        app.select_row(2);
        if depth % 2 == 0 {
            app.open_vk_album(app.vk.items[2].album_webpage_url().unwrap().to_string());
        } else {
            app.open_vk_artist(app.vk.items[2].artist_webpage_url().unwrap().to_string());
        }
    }
    assert_eq!(app.vk.back.len(), 8, "history remains bounded");
    assert!(app.save_session());
    let saved = app.store.session().unwrap().unwrap();
    assert_eq!(saved.vk_search_kind, VkSearchKind::Albums);
    assert_eq!(saved.vk_search_text, "root albums");
    assert_eq!(
        saved.vk_selected_row,
        Some(1),
        "the saved row belongs to the root search"
    );
    for _ in 0..8 {
        assert!(app.back_vk_catalogue());
    }
    assert!(!app.back_vk_catalogue());
    assert_eq!(app.vk.results_kind, VkSearchKind::Albums);
    assert_eq!(app.view.search_query, "root albums");
    assert_eq!(app.view.selected, 1);
    assert_eq!(app.view.rows[1].title, "Album 2");
}
