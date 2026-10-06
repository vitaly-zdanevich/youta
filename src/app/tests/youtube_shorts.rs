//! Shared Shorts visibility must not change canonical search or playback identities.

use super::*;

/// Creates an isolated saved-search projection with a Short between two ordinary videos.
fn fixture(show_shorts: bool) -> (tempfile::TempDir, AppController) {
    let temporary = crate::test_support::canonical_tempdir("YouTube Shorts search");
    let mut config = Config::for_dir(temporary.path().join("config"));
    config.ui.show_youtube_shorts = show_shorts;
    let mut controller = AppController::new(
        config,
        StateStore::open_in_memory().expect("state"),
        None,
        None,
    );
    controller.view.screen = Screen::Search;
    controller.youtube_search_request = Some(SearchRequest::new("fixture", SearchTarget::Videos));
    controller.youtube_results = [
        (
            "dQw4w9WgXcQ",
            "First standard",
            VideoOrientation::Horizontal,
        ),
        ("shortvideo1", "Middle Short", VideoOrientation::Vertical),
        ("thirdvideo1", "Last standard", VideoOrientation::Unknown),
    ]
    .into_iter()
    .map(|(id, title, orientation)| {
        let mut video = subscription_video_summary();
        video.video_id = id.to_owned();
        video.title = title.to_owned();
        video.orientation = orientation;
        video.webpage_url = url::Url::parse(&youtube_video_url(id)).ok();
        SearchItem::Video(video)
    })
    .collect();
    controller.refresh_youtube_rows();
    (temporary, controller)
}

/// The existing action persists the shared preference while keeping selection and raw pages.
#[test]
fn youtube_search_shorts_toggle_preserves_selection_and_canonical_results() {
    let (_temporary, mut controller) = fixture(true);
    controller.view.selected = 2;
    controller.request_selected_details();
    let original = controller.youtube_results.clone();
    let selected = controller.selected_queue_item().unwrap().media.id;
    controller.current_media = Some(selected.clone());
    let (requests, received) = unbounded();
    controller.provider_requests = Some(requests);
    controller.youtube_provider_available = true;

    controller.dispatch(UiAction::ToggleSubscriptionShorts);

    assert!(!controller.config.ui.show_youtube_shorts);
    assert!(!controller.view.subscriptions.show_youtube_shorts);
    assert_eq!(controller.view.rows.len(), 2);
    assert_eq!(controller.view.selected, 1);
    assert_eq!(controller.selected_queue_item().unwrap().media.id, selected);
    assert_eq!(controller.current_media.as_ref(), Some(&selected));
    assert_eq!(
        controller.current_url().as_deref(),
        Some("https://www.youtube.com/watch?v=thirdvideo1")
    );
    assert_eq!(controller.youtube_results, original);
    assert!(
        received.try_recv().is_err(),
        "toggling a surviving row needs no provider request"
    );
    assert!(
        std::fs::read_to_string(controller.config.config_file())
            .unwrap()
            .contains("show_youtube_shorts = false")
    );

    controller.dispatch(UiAction::ToggleSubscriptionShorts);
    assert!(controller.view.subscriptions.show_youtube_shorts);
    assert_eq!(controller.view.rows.len(), 3);
    assert_eq!(controller.view.selected, 2);
    assert_eq!(controller.selected_queue_item().unwrap().media.id, selected);
}

/// Selected and neighboring search videos always resolve through canonical indices.
#[test]
fn youtube_search_shorts_projection_keeps_actions_and_autoplay_aligned() {
    let (_temporary, mut controller) = fixture(false);
    assert_eq!(controller.view.rows.len(), 2);
    controller.view.selected = 1;
    let selected = controller.selected_queue_item().unwrap();
    assert_eq!(selected.media.id.external_id, "thirdvideo1");
    assert_eq!(
        controller.autoplay_origin_for_media(&selected.media.id),
        Some(AutoplayOrigin::YouTube {
            generation: controller.search_generation,
            index: 2,
        })
    );
    for (start, direction, expected_index, expected_id) in [
        (0, ListStepDirection::Forward, 2, "thirdvideo1"),
        (2, ListStepDirection::Backward, 0, "dQw4w9WgXcQ"),
    ] {
        let step = controller.neighbour_autoplay_step(
            &AutoplayOrigin::YouTube {
                generation: controller.search_generation,
                index: start,
            },
            direction,
        );
        let AutoplayStep::Play { item, origin } = step else {
            panic!("visible neighbor");
        };
        assert_eq!(item.media.id.external_id, expected_id);
        assert_eq!(
            origin,
            AutoplayOrigin::YouTube {
                generation: controller.search_generation,
                index: expected_index
            }
        );
    }
    controller.playback_queue.items = vec![selected];
    controller.playback_queue.current_index = Some(0);
    controller.view.selected = 0;
    controller.show_now_playing();
    assert_eq!(controller.view.selected, 1);
    assert_eq!(
        controller.view.details.as_ref().unwrap().title,
        "Last standard"
    );
}

/// A hidden-only page stays navigable, but toggling never launches a speculative page crawl.
#[test]
fn youtube_search_shorts_empty_projection_can_page_forward_explicitly() {
    let (_temporary, mut controller) = fixture(true);
    controller.youtube_results = vec![controller.youtube_results[1].clone()];
    controller.next_youtube_page = Some(2);
    controller.refresh_youtube_rows();
    controller.request_selected_details();
    let (requests, received) = unbounded();
    controller.provider_requests = Some(requests);
    controller.youtube_provider_available = true;

    controller.dispatch(UiAction::ToggleSubscriptionShorts);

    assert!(controller.view.rows.is_empty());
    assert!(controller.view.details.is_none());
    assert!(controller.selected_youtube_item().is_none());
    assert!(controller.selected_queue_item().is_err());
    assert!(controller.current_url().is_none());
    assert!(received.try_recv().is_err());
    controller.dispatch(UiAction::MoveSelection(-10));
    assert!(received.try_recv().is_err());
    controller.dispatch(UiAction::MoveSelection(10));
    let ProviderRequest::Search { request, .. } = received.try_recv().expect("explicit next page")
    else {
        panic!("search page request");
    };
    assert_eq!(request.page, 2);
    controller.dispatch(UiAction::MoveSelection(10));
    assert!(
        received.try_recv().is_err(),
        "only one page may be in flight"
    );
}

/// Late orientation proof replaces now-hidden Details without assigning another row's identity.
#[test]
fn youtube_search_shorts_details_enrichment_reselects_visible_video() {
    let (_temporary, mut controller) = fixture(false);
    if let SearchItem::Video(video) = &mut controller.youtube_results[0] {
        video.orientation = VideoOrientation::Unknown;
    }
    controller.refresh_youtube_rows();
    controller.request_selected_details();
    let mut details = subscription_video_details("Newly proved Short");
    details.orientation = VideoOrientation::Vertical;

    controller.handle_provider_response(ProviderResponse::Details {
        generation: controller.details_generation,
        result: Ok(details),
    });

    assert_eq!(controller.view.rows.len(), 1);
    assert_eq!(
        controller
            .selected_queue_item()
            .unwrap()
            .media
            .id
            .external_id,
        "thirdvideo1"
    );
    assert_eq!(
        controller.view.details.as_ref().unwrap().title,
        "Last standard"
    );
    assert_eq!(controller.youtube_results.len(), 3);
}

/// Shared visibility never removes explicit URLs, unrelated Local rows, or channel results.
#[test]
fn youtube_search_shorts_toggle_preserves_direct_local_and_channel_routes() {
    let (_temporary, mut controller) = fixture(true);
    controller.youtube_search_request = None;
    controller.youtube_results = vec![controller.youtube_results[1].clone()];
    controller.refresh_youtube_rows();
    controller.dispatch(UiAction::ToggleSubscriptionShorts);
    assert!(!controller.config.ui.show_youtube_shorts);
    assert_eq!(controller.view.rows.len(), 1);
    assert_eq!(
        controller
            .selected_queue_item()
            .unwrap()
            .media
            .id
            .external_id,
        "shortvideo1"
    );

    controller.local_results = vec![local_media_item_stub(
        fixture_absolute("/music/local.flac"),
        Some(0),
    )];
    controller.refresh_local_rows();
    let local_rows = controller.view.rows.clone();
    controller.dispatch(UiAction::ToggleSubscriptionShorts);
    assert!(controller.config.ui.show_youtube_shorts);
    assert_eq!(controller.view.rows, local_rows);

    controller.local_results.clear();
    controller.youtube_search_request = Some(SearchRequest::new("channel", SearchTarget::Channels));
    controller.youtube_results = vec![SearchItem::Channel(ChannelSummary {
        channel_id: "UCfixture".to_owned(),
        name: "Channel".to_owned(),
        description: String::new(),
        subscriber_count: None,
        video_count: None,
        created_at: None,
        auto_generated: false,
        thumbnails: Vec::new(),
        webpage_url: None,
    })];
    controller.refresh_youtube_rows();
    controller.dispatch(UiAction::ToggleSubscriptionShorts);
    assert!(!controller.config.ui.show_youtube_shorts);
    assert_eq!(controller.view.rows.len(), 1);
    assert!(matches!(
        controller.selected_youtube_item(),
        Some(SearchItem::Channel(_))
    ));
}

/// Returning from another tab uses its rebased cursor, not a stale visible row at that offset.
#[test]
fn youtube_search_shorts_parked_selection_survives_subscription_toggle() {
    let (_temporary, mut controller) = fixture(true);
    controller.youtube_results.swap(0, 1);
    controller.refresh_youtube_rows();
    controller.view.selected = 2;
    controller.show_screen(Screen::Subscriptions);
    controller.view.subscriptions.layout = SubscriptionsLayout::Split;
    controller.dispatch(UiAction::ToggleSubscriptionShorts);
    assert_eq!(controller.youtube_selected, 1);

    controller.show_screen(Screen::Search);

    assert_eq!(controller.view.selected, 1);
    assert_eq!(
        controller
            .selected_queue_item()
            .unwrap()
            .media
            .id
            .external_id,
        "thirdvideo1"
    );
    assert_eq!(
        controller.view.details.as_ref().unwrap().title,
        "Last standard"
    );
}

/// Orientation learned on another tab rebases the parked Search cursor before it is restored.
#[test]
fn youtube_search_shorts_inactive_orientation_enrichment_rebases_selection() {
    let (_temporary, mut controller) = fixture(false);
    let mut fourth = subscription_video_summary();
    fourth.video_id = "fourthvideo".to_owned();
    fourth.title = "Fourth video".to_owned();
    fourth.orientation = VideoOrientation::Horizontal;
    controller.youtube_results.push(SearchItem::Video(fourth));
    controller.refresh_youtube_rows();
    controller.view.selected = 1;
    controller.show_screen(Screen::Subscriptions);
    let mut details = subscription_video_details("Newly proved Short");
    details.orientation = VideoOrientation::Vertical;

    controller.handle_provider_response(ProviderResponse::Details {
        generation: controller.details_generation,
        result: Ok(details),
    });

    assert_eq!(controller.youtube_selected, 0);
    controller.show_screen(Screen::Search);
    assert_eq!(
        controller
            .selected_queue_item()
            .unwrap()
            .media
            .id
            .external_id,
        "thirdvideo1"
    );
}
