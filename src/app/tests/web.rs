//! Controller regressions for session-scoped HTTP folder browsing.

use super::*;
use crate::web_browser::{WebDirectoryListing, WebEntry, WebEntryKind};

fn listing() -> WebDirectoryListing {
    let url = url::Url::parse("http://127.0.0.1:8000/music/").unwrap();
    WebDirectoryListing {
        parent: Some(url.join("../").unwrap()),
        entries: vec![
            WebEntry {
                url: url.join("album/").unwrap(),
                name: "album".into(),
                kind: WebEntryKind::Directory,
            },
            WebEntry {
                url: url.join("01.opus").unwrap(),
                name: "01.opus".into(),
                kind: WebEntryKind::Audio,
            },
            WebEntry {
                url: url.join("02.mp4").unwrap(),
                name: "02.mp4".into(),
                kind: WebEntryKind::Video,
            },
        ],
        url,
        truncated: false,
    }
}

fn accept_listing(controller: &mut AppController) {
    controller.view.search_editing = false;
    controller.web.generation = 1;
    controller.web.pending = true;
    controller.handle_web_response(1, Ok(listing()));
}

#[test]
fn web_first_visit_asks_for_url_without_reusing_youtube_query() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.view.search_query = "ambient music".into();
    controller.show_screen(Screen::Web);
    assert!(controller.view.search_editing);
    assert!(controller.view.search_query.is_empty());
    assert!(controller.view.rows.is_empty());
    controller.view.search_query = "file:///etc/".into();
    controller.submit_search_input();
    assert!(controller.view.search_editing);
    assert!(controller.web.worker.is_none());
}

#[test]
fn web_listing_has_compact_parent_folders_and_media() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.show_screen(Screen::Web);
    accept_listing(&mut controller);
    assert_eq!(controller.view.rows.len(), 4);
    assert_eq!(controller.view.rows[0].title, "..");
    assert!(controller.view.rows.iter().all(|row| row.compact));
    assert!(controller.view.rows[1].media_id.is_none());
    controller.select_row(3);
    assert_eq!(
        controller.selected_queue_item().unwrap().media.kind,
        MediaKind::Video
    );
    assert_eq!(controller.view.details.as_ref().unwrap().title, "02.mp4");
    assert!(!controller.view.search_editing);
}

#[test]
fn web_stale_and_background_responses_do_not_replace_another_tab() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.show_screen(Screen::Web);
    accept_listing(&mut controller);
    controller.handle_web_response(0, Err("stale failure".into()));
    assert_eq!(controller.view.rows.len(), 4);
    controller.show_screen(Screen::Playlists);
    let rows = controller.view.rows.clone();
    controller.web.pending = true;
    controller.web.generation = 2;
    controller.handle_web_response(2, Ok(listing()));
    assert_eq!(controller.view.screen, Screen::Playlists);
    assert_eq!(controller.view.rows, rows);
    controller.show_screen(Screen::Web);
    assert_eq!(controller.view.rows.len(), 4);
}

#[test]
fn web_signed_urls_play_but_are_not_saved_in_session_history_or_playlist() {
    let (mut controller, played, _, events) = controller_with_mock_lifecycle([], []);
    controller.show_screen(Screen::Web);
    let mut value = listing();
    value.entries[1].url.set_query(Some("token=secret-fixture"));
    controller.view.search_editing = false;
    controller.web.generation = 1;
    controller.web.pending = true;
    controller.handle_web_response(1, Ok(value));
    controller.select_row(2);
    let item = controller.selected_queue_item().unwrap();
    assert!(!item.media.id.external_id.contains("secret-fixture"));
    assert!(history_replay_locator(&item).is_none());
    assert!(playlist_snapshot_from_queue_item(&item).is_err());
    controller.activate_selection();
    assert!(
        played.lock().unwrap().played[0]
            .location
            .contains("token=secret-fixture")
    );
    assert!(played.lock().unwrap().played[0].bypass_ytdl);
    events
        .lock()
        .unwrap()
        .extend([PlaybackEvent::MediaLoaded, PlaybackEvent::PlaybackStarted]);
    controller.drain_player_events(Duration::ZERO);
    assert!(controller.persist_position());
    let history = controller.store.history(false, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].replay_locator.is_none());
    assert!(
        !serde_json::to_string(&history)
            .unwrap()
            .contains("secret-fixture")
    );
    let progress = controller.store.progress(&item.media.id).unwrap().unwrap();
    assert!(
        !serde_json::to_string(&progress)
            .unwrap()
            .contains("secret-fixture")
    );
    assert!(controller.save_session());
    let saved = controller.store.session().unwrap().unwrap();
    assert!(
        !serde_json::to_string(&saved)
            .unwrap()
            .contains("secret-fixture")
    );
}

#[test]
fn web_sequential_playback_keeps_snapshot_after_browsing_elsewhere() {
    let (mut controller, played, _, events) = controller_with_mock_lifecycle([], []);
    controller.show_screen(Screen::Web);
    accept_listing(&mut controller);
    controller.config.playback.autoplay = true;
    controller.view.autoplay = true;
    controller.select_row(2);
    controller.activate_selection();
    controller.web.listing = None;
    controller.show_screen(Screen::Playlists);
    events.lock().unwrap().extend([
        PlaybackEvent::MediaLoaded,
        PlaybackEvent::PlaybackStarted,
        PlaybackEvent::Ended(PlaybackEnd {
            reason: PlaybackEndReason::Eof,
            error: None,
            file_error: None,
            diagnostic: None,
        }),
    ]);
    controller.tick();
    let state = played.lock().unwrap();
    assert_eq!(state.played.len(), 2);
    assert!(state.played[1].location.ends_with("02.mp4"));
    assert!(state.played.iter().all(|input| input.bypass_ytdl));
}

#[test]
fn web_previous_next_skip_folders_and_stop_at_list_edges() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.show_screen(Screen::Web);
    accept_listing(&mut controller);
    let item = super::super::web::queue_item_from_web(&listing().entries[2]).unwrap();
    let origin = controller
        .autoplay_origin_for_media(&item.media.id)
        .unwrap();
    assert!(matches!(
        controller.next_autoplay_step(&origin),
        AutoplayStep::Exhausted
    ));
    match controller.neighbour_autoplay_step(&origin, ListStepDirection::Backward) {
        AutoplayStep::Play { item, .. } => assert_eq!(item.media.title, "01.opus"),
        _ => panic!("expected previous media"),
    }
}

#[test]
fn web_response_preserves_an_in_progress_url_draft() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.show_screen(Screen::Web);
    accept_listing(&mut controller);
    controller.begin_search_input();
    controller.view.search_query = "https://example.org/new-draft/".into();
    controller.view.search_cursor_byte = 12;
    controller.web.pending = true;
    controller.web.generation = 2;
    controller.handle_web_response(2, Ok(listing()));
    assert!(controller.view.search_editing);
    assert_eq!(
        controller.view.search_query,
        "https://example.org/new-draft/"
    );
    assert_eq!(controller.view.search_cursor_byte, 12);
    assert_eq!(controller.web.query, listing().url.as_str());
}

#[test]
fn web_cancel_and_tab_switch_do_not_accept_an_unsubmitted_url() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.show_screen(Screen::Web);
    accept_listing(&mut controller);
    controller.begin_search_input();
    controller.view.search_query = "https://example.org/not-submitted/".into();
    controller.cancel_search_input();
    assert_eq!(controller.view.search_query, listing().url.as_str());
    controller.begin_search_input();
    controller.view.search_query = "https://example.org/also-not-submitted/".into();
    controller.show_screen(Screen::Playlists);
    controller.show_screen(Screen::Web);
    assert_eq!(controller.web.query, listing().url.as_str());
    assert_eq!(controller.view.search_query, listing().url.as_str());
}

#[test]
fn web_queryless_items_can_be_saved_as_playlist_snapshots() {
    let item = super::super::web::queue_item_from_web(&listing().entries[1]).unwrap();
    let snapshot = playlist_snapshot_from_queue_item(&item).unwrap();
    assert_eq!(snapshot.id, item.media.id);
    assert_eq!(snapshot.replay_locator, item.playback_location);
}
