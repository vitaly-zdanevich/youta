//! Finite public-metadata mocks exercise catalogue routing without real HTTP or playback.

use super::*;
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Default)]
struct Transport {
    responses: Mutex<VecDeque<Value>>,
    requests: Mutex<Vec<url::Url>>,
}

impl crate::providers::soundcloak::SoundcloakTransport for Transport {
    fn fetch(&self, url: &url::Url, _: usize) -> Result<Vec<u8>, crate::providers::ProviderError> {
        self.requests.lock().unwrap().push(url.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .map(|value| serde_json::to_vec(&value).unwrap())
            .ok_or(crate::providers::ProviderError::HttpStatus(503))
    }
}

fn user() -> Value {
    json!({"kind":"user", "id":1, "username":"Fixture Artist", "permalink":"fixture-artist",
        "permalink_url":"https://soundcloud.com/fixture-artist", "track_count":100})
}

fn track(id: u64) -> Value {
    json!({"kind":"track", "id":id, "title":format!("Track {id}"),
        "permalink_url":format!("https://soundcloud.com/guest/track-{id}"),
        "user":{"username":"Guest", "permalink":"guest", "permalink_url":"https://soundcloud.com/guest"},
        "policy":"ALLOW", "streamable":true, "duration":120_000,
        "media":{"transcodings":[{"snipped":false,"format":{"protocol":"hls","mime_type":"audio/mpeg"}}]}})
}

fn album() -> Value {
    json!({"kind":"playlist", "id":10, "title":"Fixture album", "is_album":true, "set_type":"album",
        "permalink_url":"https://soundcloud.com/fixture-artist/sets/release", "user":user(),
        "track_count":3, "tracks":[track(1), {"id":2}, {"id":3}]})
}

fn next(kind: &str, offset: &str, limit: usize) -> String {
    format!("https://api-v2.soundcloud.com/users/1/{kind}?offset={offset}&limit={limit}")
}

fn controller(responses: Vec<Value>) -> (AppController, Arc<Transport>) {
    let mut app = super::super::tests::controller();
    let transport = Arc::new(Transport {
        responses: Mutex::new(responses.into()),
        ..Transport::default()
    });
    app.soundcloud.client = Some(
        SoundcloakClient::with_transport(
            url::Url::parse("https://soundcloak.example/prefix/").unwrap(),
            transport.clone(),
        )
        .unwrap(),
    );
    app.view.search_query = "saved search".into();
    app.soundcloud.query = app.view.search_query.clone();
    app.soundcloud.submitted_query = app.view.search_query.clone();
    super::super::tests::complete(
        &mut app,
        0,
        vec![
            super::super::tests::track("one"),
            super::super::tests::track("two"),
        ],
        Some(2),
    );
    app.view.selected = 1;
    app.update_soundcloud_detail();
    (app, transport)
}

/// Joins only deterministic local fixture workers, with an explicit deadline.
fn settle(app: &mut AppController) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.poll_soundcloud_worker();
        if !app.soundcloud_catalog_busy()
            && app.soundcloud.worker.is_none()
            && app.soundcloud.pending.is_none()
        {
            break;
        }
        assert!(Instant::now() < deadline, "finite fixture work completed");
        thread::yield_now();
    }
}

/// Large descriptions must evict old snapshots before the count-only cap is reached.
#[test]
fn soundcloud_artist_history_byte_budget_preserves_the_nearest_back_destination() {
    let (mut app, _) = controller(Vec::new());
    let mut track = super::super::tests::track("large");
    track.description = Some("x".repeat(64 * 1024));
    app.soundcloud.items = vec![track; 20];
    for index in 0..4 {
        app.soundcloud.query = format!("snapshot {index}");
        app.push_soundcloud_snapshot();
    }
    assert!(
        app.soundcloud.catalog.history.len() < 4,
        "large snapshots need a byte budget, not only an eight-entry limit"
    );
    assert!(
        app.soundcloud
            .catalog
            .history
            .iter()
            .map(|snapshot| snapshot.retained_bytes)
            .sum::<usize>()
            <= MAX_HISTORY_BYTES
    );
    assert!(app.go_back_soundcloud_catalog());
    assert_eq!(app.soundcloud.query, "snapshot 3");
    assert_eq!(app.soundcloud.items.len(), 20);
    assert!(app.soundcloud.worker.is_none());
    assert!(app.soundcloud.catalog.worker.is_none());
}

/// An oversized retained album must not evict the nearest small valid destination.
#[test]
fn soundcloud_artist_history_byte_budget_accounts_for_shared_album_metadata() {
    let (mut app, _) = controller(Vec::new());
    app.soundcloud.query = "nearest valid search".into();
    app.push_soundcloud_snapshot();
    let mut track = super::super::tests::track("large");
    track.description = Some("x".repeat(64 * 1024));
    app.soundcloud.items.clear();
    app.soundcloud.catalog.route = Route::Album {
        details: Arc::new(SoundcloakAlbumDetails {
            album: SoundcloakAlbum {
                id: "10".into(),
                title: "Large album".into(),
                webpage_url: url::Url::parse("https://soundcloud.com/artist/sets/large").unwrap(),
                artist: "Artist".into(),
                artist_url: None,
                artwork_url: None,
                track_count: Some(100),
                release_type: Some("album".into()),
            },
            tracks: vec![
                SoundcloakAlbumTrack {
                    id: track.id.clone(),
                    track: Some(track)
                };
                100
            ],
        }),
        slots: Vec::new(),
        next: Some(0),
    };
    app.soundcloud.query = "oversized album".into();
    assert!(
        app.soundcloud_history_weight().is_none(),
        "preflight must reject this album before cloning a snapshot"
    );
    app.push_soundcloud_snapshot();
    assert_eq!(app.soundcloud.catalog.history.len(), 1);
    assert!(app.go_back_soundcloud_catalog());
    assert_eq!(app.soundcloud.query, "nearest valid search");
    assert!(matches!(app.soundcloud.catalog.route, Route::Search));
}

/// Small metadata still obeys the independent eight-location Back bound.
#[test]
fn soundcloud_artist_history_byte_budget_keeps_the_eight_location_limit() {
    let (mut app, _) = controller(Vec::new());
    for index in 0..10 {
        app.soundcloud.query = format!("snapshot {index}");
        app.push_soundcloud_snapshot();
    }
    assert_eq!(app.soundcloud.catalog.history.len(), 8);
    assert_eq!(
        app.soundcloud.catalog.history.front().unwrap().query,
        "snapshot 2"
    );
    assert!(app.go_back_soundcloud_catalog());
    assert_eq!(app.soundcloud.query, "snapshot 9");
}

/// Debug traversal stops at the bound without allocating or retaining formatted text.
#[test]
fn soundcloud_artist_history_byte_budget_writer_stops_at_the_first_limit() {
    struct Verbose<'a> {
        text: &'a str,
        writes: std::cell::Cell<usize>,
    }
    impl std::fmt::Debug for Verbose<'_> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for _ in 0..1000 {
                self.writes.set(self.writes.get() + 1);
                formatter.write_str(self.text)?;
            }
            Ok(())
        }
    }
    let text = "x".repeat(64 * 1024);
    let value = Verbose {
        text: &text,
        writes: std::cell::Cell::new(0),
    };
    let mut bytes = HistoryBytes(0);
    assert!(std::fmt::write(&mut bytes, format_args!("{value:?}")).is_err());
    assert_eq!(value.writes.get(), MAX_HISTORY_BYTES / 4 / text.len() + 1);
    assert_eq!(bytes.0, MAX_HISTORY_BYTES / 4);
    assert_eq!(
        std::mem::size_of::<HistoryBytes>(),
        std::mem::size_of::<usize>()
    );
}

#[test]
fn soundcloud_artist_mock_tracks_and_back_restore_exact_cached_search() {
    let (mut app, transport) = controller(vec![
        user(),
        json!({"collection":[track(1)], "next_href":null}),
    ]);
    app.view.external_opener_available = false;
    app.dispatch(UiAction::OpenSoundCloudArtist(
        "https://soundcloud.com/fixture-artist".into(),
    ));
    assert!(app.view.soundcloud_back_available);
    assert_eq!(app.view.search_activity, Some(SearchActivity::SoundCloud));
    assert!(
        transport.requests.lock().unwrap().is_empty(),
        "dispatch only queues visible intent"
    );
    settle(&mut app);
    assert_eq!(app.view.rows[0].title, "Track 1");
    assert_eq!(
        app.view.rows[0].media_id.as_ref().unwrap().external_id,
        "https://soundcloud.com/guest/track-1"
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    app.dispatch(UiAction::GoBack);
    assert_eq!(app.view.search_query, "saved search");
    assert_eq!(app.view.selected, 1);
    assert_eq!(app.view.rows[1].title, "Track two");
    assert_eq!(app.soundcloud.next_page, Some(2));
    assert!(!app.view.soundcloud_back_available);
    assert_eq!(
        transport.requests.lock().unwrap().len(),
        2,
        "Back is cache-only"
    );
}

#[test]
fn soundcloud_artist_hidden_intent_and_invalid_profiles_do_not_fetch() {
    let (mut app, transport) = controller(vec![user(), json!({"collection":[], "next_href":null})]);
    for url in [
        "https://evil.example/artist",
        "https://soundcloud.com/user/track",
        "https://soundcloud.com/user?secret_token=s-private",
    ] {
        app.open_soundcloud_artist(url.into(), false);
        assert!(!app.view.soundcloud_back_available);
    }
    app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), false);
    app.show_screen(Screen::Search);
    app.poll_soundcloud_worker();
    assert!(transport.requests.lock().unwrap().is_empty());
    app.show_screen(Screen::SoundCloud);
    settle(&mut app);
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    assert!(app.view.rows.is_empty());
    assert_eq!(app.view.details.as_ref().unwrap().title, "Fixture Artist");
}

#[test]
fn soundcloud_artist_albums_open_ordered_tracks_and_keep_unavailable_slots() {
    let (mut app, transport) = controller(vec![
        user(),
        json!({"collection":[album()],"next_href":null}),
        album(),
        json!([track(3)]),
    ]);
    app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), true);
    settle(&mut app);
    assert_eq!(app.view.rows[0].title, "Fixture album");
    assert!(app.view.rows[0].media_id.is_none());
    app.view.selected = 0;
    app.activate_soundcloud_selection();
    settle(&mut app);
    assert_eq!(
        app.view
            .rows
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        vec!["Track 1", "Track 2 · unavailable", "Track 3"]
    );
    app.view.selected = 1;
    app.update_soundcloud_detail();
    assert!(app.selected_soundcloud_queue_item().is_err());
    assert!(app.view.details.as_ref().unwrap().media_id.is_none());
    app.view.selected = 2;
    app.update_soundcloud_detail();
    assert_eq!(
        app.selected_soundcloud_queue_item()
            .unwrap()
            .media
            .webpage_url
            .as_str(),
        "https://soundcloud.com/guest/track-3"
    );
    assert!(app.view.details.as_ref().unwrap().links.iter().any(|link| matches!(&link.internal_target, Some(DetailLinkInternalTarget::SoundCloudArtist(url)) if url == "https://soundcloud.com/guest")));
    app.go_back();
    assert_eq!(app.view.rows[0].title, "Fixture album");
    app.go_back();
    assert_eq!(app.view.selected, 1);
    assert_eq!(transport.requests.lock().unwrap().len(), 4);
}

#[test]
fn soundcloud_artist_empty_pages_are_bounded_and_retain_owned_continuation() {
    let mut responses = vec![user()];
    for offset in 1..=4 {
        responses.push(
            json!({"collection":[], "next_href":next("tracks",&format!("cursor{offset}"),2)}),
        );
    }
    responses.push(json!({"collection":[track(9)], "next_href":null}));
    let (mut app, transport) = controller(responses);
    app.update_soundcloud_search_page_capacity(2);
    app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), false);
    settle(&mut app);
    assert_eq!(
        transport.requests.lock().unwrap().len(),
        5,
        "four raw pages after one resolve"
    );
    assert_eq!(
        app.view.rows.len(),
        1,
        "empty catalogue still has a continuation row"
    );
    assert!(app.soundcloud_catalog_has_more());
    app.update_soundcloud_search_page_capacity(90);
    app.activate_soundcloud_selection();
    settle(&mut app);
    assert_eq!(app.view.rows[0].title, "Track 9");
    assert!(!app.soundcloud_catalog_has_more());
    let requests = transport.requests.lock().unwrap();
    let last = requests.last().unwrap();
    assert!(
        last.query_pairs()
            .any(|(key, value)| key == "offset" && value == "cursor4")
    );
    assert!(
        last.query_pairs()
            .any(|(key, value)| key == "limit" && value == "2")
    );
}

#[test]
fn soundcloud_artist_continuation_does_not_steal_changed_selection() {
    let (mut app, _) = controller(vec![
        user(),
        json!({"collection":[track(1),track(2)], "next_href":next("tracks","opaque",2)}),
        json!({"collection":[track(3)], "next_href":null}),
    ]);
    app.update_soundcloud_search_page_capacity(2);
    app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), false);
    settle(&mut app);
    app.view.selected = 2;
    app.update_soundcloud_detail();
    app.activate_soundcloud_selection();
    app.view.selected = 0;
    app.update_soundcloud_detail();
    settle(&mut app);
    assert_eq!(app.view.selected, 0);
    assert_eq!(app.view.rows.len(), 3);
}

#[test]
fn soundcloud_artist_continuation_owner_is_canceled_even_after_returning_to_same_row() {
    for switch_tab in [false, true] {
        let (mut app, _) = controller(vec![
            user(),
            json!({"collection":[track(1),track(2)], "next_href":next("tracks","opaque",2)}),
            json!({"collection":[track(3),track(4)], "next_href":null}),
        ]);
        app.update_soundcloud_search_page_capacity(2);
        app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), false);
        settle(&mut app);
        app.view.selected = 2;
        app.update_soundcloud_detail();
        app.activate_soundcloud_selection();
        if switch_tab {
            app.show_screen(Screen::Search);
            app.show_screen(Screen::SoundCloud);
        } else {
            app.view.selected = 0;
            app.update_soundcloud_detail();
            app.view.selected = 2;
            app.update_soundcloud_detail();
        }
        settle(&mut app);
        assert_eq!(
            app.view.selected, 2,
            "a cancelled page turn cannot reclaim selection when returning (tab={switch_tab})"
        );
        assert_eq!(app.view.rows.len(), 4);
    }
}

/// A paused metadata request exposes whether newer intents accidentally start a second lane.
struct GatedTransport {
    inner: Arc<Transport>,
    started: Sender<()>,
    release: Receiver<()>,
    count: std::sync::atomic::AtomicUsize,
}

impl crate::providers::soundcloak::SoundcloakTransport for GatedTransport {
    fn fetch(
        &self,
        url: &url::Url,
        limit: usize,
    ) -> Result<Vec<u8>, crate::providers::ProviderError> {
        if self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.started.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        crate::providers::soundcloak::SoundcloakTransport::fetch(self.inner.as_ref(), url, limit)
    }
}

#[test]
fn soundcloud_artist_worker_serializes_against_newer_text_search_and_discards_old_rows() {
    let (mut app, transport) = controller(vec![
        user(),
        json!({"collection":[track(1)],"next_href":null}),
        json!({"collection":[track(9)],"next_href":null}),
    ]);
    let (started, started_rx) = bounded(1);
    let (release, release_rx) = bounded(1);
    let gated = Arc::new(GatedTransport {
        inner: transport.clone(),
        started,
        release: release_rx,
        count: std::sync::atomic::AtomicUsize::new(0),
    });
    app.soundcloud.client = Some(
        SoundcloakClient::with_transport(
            url::Url::parse("https://soundcloak.example/").unwrap(),
            gated.clone(),
        )
        .unwrap(),
    );
    app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), false);
    app.poll_soundcloud_worker();
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    for query in ["older", "latest"] {
        app.view.search_query = query.into();
        app.submit_soundcloud_search(query.into());
    }
    assert_eq!(gated.count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        app.soundcloud.worker.is_none(),
        "new search waits for catalogue HTTP to finish"
    );
    release.send(()).unwrap();
    settle(&mut app);
    assert_eq!(app.view.rows[0].title, "Track 9");
    assert_eq!(app.view.search_query, "latest");
    assert!(!app.view.soundcloud_back_available);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2]
            .query_pairs()
            .any(|(key, value)| key == "q" && value == "latest")
    );
}

#[test]
fn soundcloud_artist_back_rejects_stale_completion_and_latest_intent_wins() {
    let (mut app, transport) = controller(vec![
        user(),
        json!({"collection":[track(1)],"next_href":null}),
    ]);
    app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), false);
    let stale = app.soundcloud.catalog.pending.as_ref().unwrap().clone();
    app.go_back();
    let artist = SoundcloakArtist {
        id: "1".into(),
        name: "Stale".into(),
        webpage_url: url::Url::parse("https://soundcloud.com/fixture-artist").unwrap(),
        track_count: None,
    };
    app.apply_soundcloud_catalog(
        stale,
        Ok(Page {
            route: Route::Tracks { artist, next: None },
            tracks: vec![super::super::tests::track("stale")],
        }),
    );
    assert_eq!(app.view.rows[1].title, "Track two");
    assert!(transport.requests.lock().unwrap().is_empty());
    for albums in [true, false, true, false] {
        app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), albums);
    }
    assert_eq!(
        app.soundcloud.catalog.history.len(),
        1,
        "pending replacements never stack blank loading pages"
    );
    settle(&mut app);
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    assert_eq!(app.view.rows[0].title, "Track 1");
}

#[test]
fn soundcloud_artist_links_tags_and_comment_authors_are_internal_and_owned() {
    let (mut app, transport) = controller(vec![]);
    let track = &mut app.soundcloud.items[1];
    track.artist_url = Some(url::Url::parse("https://soundcloud.com/fixture-artist").unwrap());
    track.tags = vec!["ambient dub".into()];
    app.update_soundcloud_detail();
    let details = app.view.details.as_ref().unwrap();
    assert!(
        details
            .links
            .iter()
            .any(|link| link.url == "https://soundcloak.example/prefix/fixture-artist")
    );
    let tag = details
        .links
        .iter()
        .position(|link| {
            matches!(
                link.internal_target,
                Some(DetailLinkInternalTarget::SoundCloudTag(_))
            )
        })
        .unwrap();
    assert!(
        !app.view
            .action_requires_external_opener(&UiAction::ActivateDetailLink(tag))
    );
    app.view.video_comments_popup = Some(VideoCommentsPopupView {
        source: SourceKind::SoundCloud,
        video_id: "stale".into(),
        video_title: "Fixture".into(),
        state: VideoCommentsPopupState::Ready,
        comments: vec![VideoCommentView {
            author_name: "Artist".into(),
            author_url: Some("https://soundcloud.com/fixture-artist".into()),
            like_count: 0,
            published: None,
            text: "Comment".into(),
        }],
        scroll_offset: 0,
    });
    app.open_soundcloud_comment_author(0);
    assert!(!app.view.soundcloud_back_available);
    app.view.video_comments_popup.as_mut().unwrap().video_id =
        app.soundcloud.items[1].webpage_url.to_string();
    app.open_soundcloud_comment_author(9);
    assert!(!app.view.soundcloud_back_available);
    app.open_soundcloud_comment_author(0);
    assert!(app.view.video_comments_popup.is_none());
    assert!(app.view.soundcloud_back_available);
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[test]
fn soundcloud_artist_tag_search_uses_fixed_filter_and_back_restores_scope() {
    let (mut app, transport) = controller(vec![
        json!({"collection":[track(8)], "next_href":null}),
        user(),
        json!({"collection":[],"next_href":null}),
    ]);
    app.search_soundcloud_tag("ambient dub".into());
    settle(&mut app);
    assert_eq!(app.view.search_query, "ambient dub");
    assert_eq!(app.soundcloud.tag.as_deref(), Some("ambient dub"));
    let first = transport.requests.lock().unwrap()[0].clone();
    assert!(
        first
            .query_pairs()
            .any(|(key, value)| key == "q" && value == "*")
    );
    assert!(
        first
            .query_pairs()
            .any(|(key, value)| key == "filter.genre_or_tag" && value == "ambient dub")
    );
    app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), false);
    settle(&mut app);
    app.go_back();
    assert_eq!(app.soundcloud.tag.as_deref(), Some("ambient dub"));
    assert_eq!(app.view.rows[0].title, "Track 8");
    app.go_back();
    assert_eq!(app.soundcloud.tag, None);
    assert_eq!(app.view.selected, 1);
    assert_eq!(app.view.search_query, "saved search");
}

#[test]
fn soundcloud_artist_reveal_cached_album_row_uses_ordered_slot_without_fetch() {
    let (mut app, transport) = controller(vec![album(), json!([track(3)])]);
    app.open_soundcloud_catalog(Request::Album {
        url: url::Url::parse("https://soundcloud.com/fixture-artist/sets/release").unwrap(),
    });
    settle(&mut app);
    let id = app.view.rows[2].media_id.clone().unwrap();
    app.open_soundcloud_artist("https://soundcloud.com/fixture-artist".into(), false);
    let before = transport.requests.lock().unwrap().len();
    assert!(app.reveal_playing_soundcloud(&id));
    assert_eq!(app.view.selected, 2);
    assert_eq!(
        app.view.details.as_ref().unwrap().media_id.as_ref(),
        Some(&id)
    );
    assert!(app.soundcloud.catalog.pending.is_none());
    assert_eq!(transport.requests.lock().unwrap().len(), before);
    let unknown = MediaId::new(
        SourceKind::SoundCloud,
        "https://soundcloud.com/other/missing",
    );
    assert!(!app.reveal_playing_soundcloud(&unknown));
}

#[test]
fn soundcloud_artist_album_duplicate_tracks_keep_order_and_autoplay_position() {
    let mut release = album();
    release["tracks"] = json!([track(1), track(2), track(1)]);
    let (mut app, _) = controller(vec![release]);
    app.update_soundcloud_search_page_capacity(2);
    app.open_soundcloud_catalog(Request::Album {
        url: url::Url::parse("https://soundcloud.com/fixture-artist/sets/release").unwrap(),
    });
    settle(&mut app);
    app.view.selected = 2;
    app.update_soundcloud_detail();
    app.activate_soundcloud_selection();
    settle(&mut app);
    assert_eq!(
        app.soundcloud.items.len(),
        3,
        "album occurrences must not be canonical-ID deduplicated"
    );
    app.view.selected = 2;
    app.update_soundcloud_detail();
    let id = app.view.rows[2].media_id.as_ref().unwrap();
    let Some(AutoplayOrigin::SoundCloud { items, index }) = app.soundcloud_autoplay_origin(id)
    else {
        panic!("album autoplay origin");
    };
    assert_eq!(items.len(), 3);
    assert_eq!(
        index, 2,
        "selected repeated track remains the third occurrence"
    );
}
