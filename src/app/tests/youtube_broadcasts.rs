//! Broadcast evidence survives row projection, incomplete refreshes, and restart snapshots.

use super::*;

/// Seeds matching search and subscription snapshots without contacting a provider.
fn fixture() -> (tempfile::TempDir, AppController) {
    let temporary = crate::test_support::canonical_tempdir("YouTube broadcasts");
    let mut controller = AppController::new(
        Config::for_dir(temporary.path().join("config")),
        StateStore::open_in_memory().expect("state"),
        None,
        None,
    );
    controller.view.screen = Screen::Search;
    let request = SearchRequest::new("fixture", SearchTarget::Videos);
    controller.youtube_search_request = Some(request.clone());
    controller.youtube_results = vec![SearchItem::Video(subscription_video_summary())];
    controller
        .store
        .save_youtube_search(
            &SavedYouTubeSearch {
                request,
                results: controller.youtube_results.clone(),
                next_page: None,
            },
            1,
        )
        .unwrap();
    controller.cache_subscription_video_page(
        "UCfixture",
        SearchPage {
            page: 1,
            items: controller.youtube_results.clone(),
            next_page: None,
        },
    );
    controller
        .persist_subscription_video_snapshot("UCfixture")
        .unwrap();
    controller.refresh_youtube_rows();
    (temporary, controller)
}

/// Reads the single fixture video's flags from any summary projection.
fn flags(items: &[SearchItem]) -> (bool, bool) {
    let SearchItem::Video(video) = &items[0] else {
        panic!("video fixture")
    };
    (video.live, video.was_live)
}

/// Detail proof survives incomplete responses, but a current broadcast clears stale past state.
#[test]
fn youtube_broadcast_details_preserve_evidence_and_persist_snapshots() {
    let (_temporary, mut controller) = fixture();
    for (incoming_live, incoming_past, expected_past) in [
        (false, true, true),
        (false, false, true),
        (true, true, false),
        (false, false, false),
    ] {
        let mut details = subscription_video_details("Broadcast fixture");
        details.live = incoming_live;
        details.was_live = incoming_past;
        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(details),
        });
        let expected = (incoming_live, expected_past);
        assert_eq!(flags(&controller.youtube_results), expected);
        assert_eq!(
            flags(&controller.subscription_video_cache["UCfixture"].items),
            expected
        );
        let cached = &controller.youtube_video_details_cache["dQw4w9WgXcQ"];
        assert_eq!((cached.live, cached.was_live), expected);
        let row = serde_json::to_value(&controller.view.rows[0]).unwrap();
        assert_eq!(row["was_live"], expected_past);
        assert_eq!(row["live"], incoming_live);
        assert_eq!(
            flags(&controller.store.youtube_search().unwrap().unwrap().results),
            expected
        );
        let snapshot = controller
            .store
            .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
            .unwrap()
            .unwrap();
        assert_eq!(flags(&snapshot.items), expected);
    }
}

/// Page-one replacement retains confirmed history; live pages cannot resurrect old cache proof.
#[test]
fn youtube_broadcast_page_refresh_preserves_evidence_until_current_live() {
    let (_temporary, mut controller) = fixture();
    let mut details = subscription_video_details("Confirmed past stream");
    details.was_live = true;
    controller.handle_provider_response(ProviderResponse::Details {
        generation: controller.details_generation,
        result: Ok(details),
    });
    for (live, expected_past) in [(false, true), (true, false), (false, false)] {
        let mut video = subscription_video_summary();
        video.live = live;
        let request = controller.youtube_search_request.clone().unwrap();
        controller.handle_provider_response(ProviderResponse::Search {
            generation: controller.search_generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(video)],
                next_page: None,
            }),
        });
        assert_eq!(flags(&controller.youtube_results), (live, expected_past));
        let mut unknown = subscription_video_summary();
        unknown.live = live;
        controller.cache_subscription_video_page(
            "UCfixture",
            SearchPage {
                page: 1,
                items: vec![SearchItem::Video(unknown)],
                next_page: None,
            },
        );
        assert_eq!(
            flags(&controller.subscription_video_cache["UCfixture"].items),
            (live, expected_past)
        );
    }
}

/// Pre-feature snapshots stay readable and do not invent past broadcasts.
#[test]
fn youtube_broadcast_legacy_summary_defaults_to_unknown() {
    let mut serialized = serde_json::to_value(subscription_video_summary()).unwrap();
    serialized.as_object_mut().unwrap().remove("was_live");
    let summary: VideoSummary = serde_json::from_value(serialized).unwrap();
    assert!(!summary.was_live);
    assert_eq!(
        serde_json::to_value(RowView::default()).unwrap()["was_live"],
        false
    );
}

/// A repeated video can carry its only past-broadcast proof in a not-yet-promoted page.
#[test]
fn youtube_broadcast_staged_only_evidence_survives_incomplete_page() {
    let (_temporary, mut controller) = fixture();
    let mut staged = subscription_video_summary();
    staged.was_live = true;
    controller.pending_subscription_refresh = Some(PendingSubscriptionRefresh {
        channel_id: "UCfixture".to_owned(),
        selected_video_id: None,
        fallback_index: 0,
        staged_cache: Some(CachedSubscriptionVideos {
            items: vec![SearchItem::Video(staged)],
            ..CachedSubscriptionVideos::default()
        }),
        report_errors: false,
    });
    let mut incoming = vec![SearchItem::Video(subscription_video_summary())];

    controller.merge_youtube_broadcast_summaries(&mut incoming);

    assert_eq!(flags(&incoming), (false, true));
    let staged = controller
        .pending_subscription_refresh
        .as_ref()
        .unwrap()
        .staged_cache
        .as_ref()
        .unwrap();
    assert_eq!(flags(&staged.items), (false, true));
}

/// Detail enrichment may update RAM while disabled history keeps saved searches absent.
#[test]
fn youtube_broadcast_history_disabled_does_not_recreate_saved_search() {
    let (_temporary, mut controller) = fixture();
    controller.config.persistence.save_playback_history = false;
    controller.store.clear_youtube_search().unwrap();
    let mut details = subscription_video_details("Private past broadcast");
    details.was_live = true;
    controller.handle_provider_response(ProviderResponse::Details {
        generation: controller.details_generation,
        result: Ok(details),
    });
    assert_eq!(flags(&controller.youtube_results), (false, true));
    assert!(controller.store.youtube_search().unwrap().is_none());
}
