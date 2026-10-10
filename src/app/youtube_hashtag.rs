//! Cached return navigation for internal `YouTube` hashtag searches.

use super::*;

/// Large results move into a short process-local stack; older origins are discarded.
const MAX_HASHTAG_LOCATIONS: usize = 16;

/// One displaced page and the `YouTube` adapter state replaced by its hashtag search.
///
/// The adapter is retained even when the visible origin is another tab. Playback,
/// download state and provider workers remain owned by the live controller.
pub(super) struct HashtagLocation {
    details: DetailNavigationSnapshot,
    detail_back: VecDeque<DetailNavigationSnapshot>,
    detail_forward: VecDeque<DetailNavigationSnapshot>,
    rows: Vec<RowView>,
    query: String,
    cursor: usize,
    search_kind: SearchKind,
    sort: YouTubeSearchSort,
    creative_commons_only: bool,
    show_youtube_shorts: bool,
    youtube_query: String,
    youtube_selected: usize,
    items: Vec<SearchItem>,
    request: Option<SearchRequest>,
    next_page: Option<u32>,
    direct: Option<DirectSourceInput>,
    resolved: Option<ResolvedDirectMedia>,
    local: Vec<LocalMediaItem>,
    start_override: Option<u64>,
    subscription_channel: Option<String>,
    subscription_cache: Option<(String, CachedSubscriptionVideos)>,
    #[cfg(feature = "rss")]
    subscription_rss: Option<String>,
    provider_generation: u64,
}

impl AppController {
    /// Searches a validated hashtag while retaining its origin for ordinary Back.
    pub(super) fn search_youtube_hashtag(&mut self, tag: &str) {
        if tag.len() > 400 {
            "Invalid YouTube hashtag".clone_into(&mut self.view.status_line);
            return;
        }
        let query = format!("#{tag}");
        if !matches!(parse_description_links(&query).as_slice(), [link]
			if link.start_byte == 0 && link.end_byte == query.len()
				&& matches!(&link.target, LinkTarget::Hashtag { tag: parsed } if parsed == tag))
        {
            "Invalid YouTube hashtag".clone_into(&mut self.view.status_line);
            return;
        }
        if !self.youtube_provider_available {
            self.open_youtube_setup();
            return;
        }

        let location = self.take_youtube_hashtag_location();
        // Ordinary page-one submissions abandon old hashtag navigation. Move the
        // stack out only for this internal transition so nested hashtags retain it.
        let mut history = std::mem::take(&mut self.youtube_hashtag_history);
        if history.len() >= MAX_HASHTAG_LOCATIONS {
            history.pop_front();
        }
        history.push_back(location);
        self.prepare_screen_transition(Screen::Search);
        self.cancel_hashtag_subscription_work();
        self.view.screen = Screen::Search;
        self.view.right_panel_mode = RightPanelMode::Details;
        self.view.search_kind = SearchKind::Videos;
        self.view.search_query = query;
        self.view.search_cursor_byte = self.view.search_query.len();
        self.view.details_focused = false;
        self.view.details_scroll = 0;
        self.view.selected_detail_link = None;
        self.view.detail_link_reveal = None;
        self.view.text_selection_mode = false;
        self.view.details_text_selection = None;
        self.clear_search_activity();
        self.submit_youtube_search(1);
        self.youtube_hashtag_history = history;
    }

    /// Moves search pages and Details, retaining a subscription copy for playback.
    fn take_youtube_hashtag_location(&mut self) -> HashtagLocation {
        let youtube_query = if self.view.screen == Screen::Search {
            self.view.search_query.clone()
        } else {
            self.youtube_search_query.clone()
        };
        let youtube_selected = if self.view.screen == Screen::Search {
            self.view.selected
        } else {
            self.youtube_selected
        };
        let subscription_source = self.active_subscription_channel_id.clone().or_else(|| {
            #[cfg(feature = "rss")]
            {
                self.active_subscription_rss_url.clone()
            }
            #[cfg(not(feature = "rss"))]
            {
                None
            }
        });
        let subscription_cache = subscription_source.and_then(|source| {
            // Subscription autoplay still reads the live cache while browsing the
            // hashtag. Keep that owner intact and retain its bounded origin copy.
            self.subscription_video_cache
                .get(&source)
                .cloned()
                .map(|cached| (source, cached))
        });
        HashtagLocation {
            details: self.take_detail_navigation_snapshot(),
            detail_back: std::mem::take(&mut self.detail_navigation_back),
            detail_forward: std::mem::take(&mut self.detail_navigation_forward),
            rows: std::mem::take(&mut self.view.rows),
            query: self.view.search_query.clone(),
            cursor: self.view.search_cursor_byte,
            search_kind: self.view.search_kind,
            sort: self.view.youtube_search_sort,
            creative_commons_only: self.view.youtube_creative_commons_only,
            show_youtube_shorts: self.config.ui.show_youtube_shorts,
            youtube_query,
            youtube_selected,
            items: std::mem::take(&mut self.youtube_results),
            request: self.youtube_search_request.take(),
            next_page: self.next_youtube_page.take(),
            direct: self.direct_item.take(),
            resolved: self.resolved_direct.take(),
            local: std::mem::take(&mut self.local_results),
            start_override: self.selected_start_override.take(),
            subscription_channel: self.active_subscription_channel_id.take(),
            subscription_cache,
            #[cfg(feature = "rss")]
            subscription_rss: self.active_subscription_rss_url.take(),
            provider_generation: self.youtube_provider_generation,
        }
    }

    /// Revokes both initial and refresh pages while their cached origin is parked.
    fn cancel_hashtag_subscription_work(&mut self) {
        self.subscription_generation = self.subscription_generation.wrapping_add(1);
        self.active_subscription_channel_id = None;
        self.pending_subscription_refresh = None;
        #[cfg(feature = "rss")]
        {
            self.active_subscription_rss_url = None;
            self.pending_rss_subscription_refresh = None;
        }
        self.clear_subscription_loading_state();
        self.subscription_automatic_prefetch_count = 0;
        self.subscription_viewport_end = None;
    }

    /// Restores a cached origin and rejects work belonging to the departed search.
    pub(super) fn restore_youtube_hashtag_location(&mut self) -> bool {
        if self.view.screen != Screen::Search {
            return false;
        }
        let Some(location) = self.youtube_hashtag_history.pop_back() else {
            return false;
        };
        if location.provider_generation != self.youtube_provider_generation {
            self.youtube_hashtag_history.clear();
            return false;
        }
        self.supersede_search_generation();
        self.prepare_screen_transition(location.details.screen);
        self.cancel_hashtag_subscription_work();
        self.youtube_search_query = location.youtube_query;
        self.youtube_selected = location.youtube_selected;
        self.youtube_results = location.items;
        self.youtube_search_request = location.request;
        self.next_youtube_page = location.next_page;
        self.direct_item = location.direct;
        self.resolved_direct = location.resolved;
        self.local_results = location.local;
        self.selected_start_override = location.start_override;
        self.view.rows = location.rows;
        self.view.search_query = location.query;
        self.view.search_cursor_byte = location.cursor;
        self.view.search_kind = location.search_kind;
        self.view.youtube_search_sort = location.sort;
        self.view.youtube_creative_commons_only = location.creative_commons_only;
        self.active_subscription_channel_id = location.subscription_channel;
        if let Some((source, cached)) = location.subscription_cache {
            self.insert_subscription_cache_value(source, cached);
        }
        #[cfg(feature = "rss")]
        {
            self.active_subscription_rss_url = location.subscription_rss;
        }
        self.restore_detail_navigation_snapshot(location.details);
        self.detail_navigation_back = location.detail_back;
        self.detail_navigation_forward = location.detail_forward;
        self.view.detail_link_reveal = None;
        if location.show_youtube_shorts != self.config.ui.show_youtube_shorts {
            self.reproject_hashtag_origin_shorts(location.show_youtube_shorts);
        }
        self.refresh_selected_playlist_state();
        self.view.status_line = "Returned from YouTube hashtag search".into();
        self.persist_restored_hashtag_search();
        true
    }

    /// Reapplies the shared Shorts preference without replaying searches or video requests.
    fn reproject_hashtag_origin_shorts(&mut self, previously_showed_shorts: bool) {
        if self.youtube_search_request.is_some()
            && self.direct_item.is_none()
            && self.resolved_direct.is_none()
            && self.local_results.is_empty()
        {
            let mut visible_count: usize = 0;
            let selection_map = self
                .youtube_results
                .iter()
                .filter_map(|item| {
                    let current = self.youtube_search_item_visible(item).then(|| {
                        let index = visible_count;
                        visible_count += 1;
                        index
                    });
                    let previously_visible = previously_showed_shorts
                        || !matches!(
                            item, SearchItem::Video(video) if youtube_video_uses_shorts_style(video)
                        );
                    previously_visible.then_some(current)
                })
                .collect::<Vec<_>>();
            self.youtube_selected = selection_map
                .get(self.youtube_selected)
                .copied()
                .flatten()
                .unwrap_or_else(|| self.youtube_selected.min(visible_count.saturating_sub(1)));
            if self.view.screen == Screen::Search {
                let previous_selected = self.view.selected;
                let selected = selection_map.get(previous_selected).copied().flatten();
                let had_selection = self.view.rows.get(previous_selected).is_some();
                self.refresh_youtube_rows();
                self.view.selected = selected.unwrap_or_else(|| {
                    previous_selected.min(self.view.rows.len().saturating_sub(1))
                });
                self.youtube_selected = self.view.selected;
                self.rebase_hashtag_detail_history(Screen::Search, &selection_map);
                if had_selection && selected.is_none() {
                    self.replace_hidden_hashtag_origin_details();
                }
            }
        }
        if self.view.screen == Screen::Subscriptions {
            self.reproject_hashtag_subscription_shorts(previously_showed_shorts);
        }
    }

    /// Restores subscription rows from their retained source under the current preference.
    fn reproject_hashtag_subscription_shorts(&mut self, previously_showed_shorts: bool) {
        let source = self.active_subscription_channel_id.as_deref().or_else(|| {
            #[cfg(feature = "rss")]
            {
                self.active_subscription_rss_url.as_deref()
            }
            #[cfg(not(feature = "rss"))]
            {
                None
            }
        });
        let Some(cached) = source.and_then(|source| self.subscription_video_cache.get(source))
        else {
            return;
        };
        let mut visible_count: usize = 0;
        let selection_map = cached
            .items
            .iter()
            .filter_map(|item| {
                let short = matches!(item,
				SearchItem::Video(video) if youtube_video_uses_shorts_style(video));
                let current = (self.config.ui.show_youtube_shorts || !short).then(|| {
                    let index = visible_count;
                    visible_count += 1;
                    index
                });
                (previously_showed_shorts || !short).then_some(current)
            })
            .collect::<Vec<_>>();
        let previous_selected = self.view.subscriptions.selected_item;
        let selected = selection_map.get(previous_selected).copied().flatten();
        let details = self.view.details.take();
        let selected_link = self.view.selected_detail_link;
        self.refresh_subscription_video_rows();
        self.view.subscriptions.selected_item = selected.unwrap_or_else(|| {
            previous_selected.min(self.view.subscriptions.items.len().saturating_sub(1))
        });
        self.view.details = details;
        self.view.selected_detail_link = selected_link;
        self.rebase_hashtag_detail_history(Screen::Subscriptions, &selection_map);
        let item_details_visible = self.view.subscriptions.route == SubscriptionRoute::Items
            || (self.view.subscriptions.layout == SubscriptionsLayout::Split
                && self.view.subscriptions.focus == SubscriptionPane::Items);
        if item_details_visible
            && selection_map.get(previous_selected).is_some()
            && selected.is_none()
        {
            self.replace_hidden_hashtag_origin_details();
        }
    }

    /// Rebases description Back/Forward locations and drops entries whose source is hidden.
    fn rebase_hashtag_detail_history(&mut self, screen: Screen, selection_map: &[Option<usize>]) {
        for history in [
            &mut self.detail_navigation_back,
            &mut self.detail_navigation_forward,
        ] {
            history.retain_mut(|snapshot| {
                if snapshot.screen != screen
                    || (screen == Screen::Subscriptions
                        && snapshot.subscription_route == SubscriptionRoute::Sources
                        && snapshot.subscription_focus == SubscriptionPane::Sources)
                {
                    return true;
                }
                let selected = if screen == Screen::Subscriptions {
                    &mut snapshot.subscription_selected_item
                } else {
                    &mut snapshot.selected
                };
                let Some(rebased) = selection_map.get(*selected).copied().flatten() else {
                    return false;
                };
                *selected = rebased;
                true
            });
        }
    }

    /// Replaces a hidden selection from owned summaries without issuing provider work.
    fn replace_hidden_hashtag_origin_details(&mut self) {
        self.clear_detail_navigation_history();
        self.previous_detail = None;
        #[cfg(feature = "wikidata")]
        self.invalidate_wikidata_lookup();
        self.view.details = self.selected_youtube_item().map(|item| {
            preliminary_detail_with_thumbnail_size(
                item,
                &self.subscription_tree,
                self.effective_youtube_thumbnail_size(),
                self.youtube_thumbnail_terminal_bounds(),
            )
        });
        self.view.right_panel_mode = RightPanelMode::Details;
        self.view.details_scroll = 0;
        self.view.selected_detail_link = None;
        self.view.details_text_selection = None;
        self.view.text_selection_mode = false;
    }

    /// Keeps restart state aligned with the restored adapter without retaining copies.
    fn persist_restored_hashtag_search(&mut self) {
        let result = if let Some(request) = self.youtube_search_request.clone() {
            let saved = SavedYouTubeSearch {
                request,
                results: std::mem::take(&mut self.youtube_results),
                next_page: self.next_youtube_page,
            };
            let result = self.save_youtube_search(&saved, unix_time());
            self.youtube_results = saved.results;
            result
        } else {
            self.store.clear_youtube_search().map(|_| ())
        };
        if let Err(error) = result {
            self.show_error("Could not restore the saved YouTube search", &error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::MAX_HASHTAG_LOCATIONS;
    use crate::app::tests::{controller_with_youtube_video_comments, linked_video_details};

    /// Seeds accepted search data without a provider worker or external playback.
    fn hashtag_controller() -> (tempfile::TempDir, AppController, Receiver<ProviderRequest>) {
        let (temporary, mut controller, requests) = controller_with_youtube_video_comments();
        controller.youtube_provider_available = true;
        controller.view.search_query = "original search".into();
        controller.youtube_search_query = controller.view.search_query.clone();
        controller.view.search_cursor_byte = controller.view.search_query.len();
        controller.view.youtube_search_sort = YouTubeSearchSort::Newest;
        controller.view.youtube_creative_commons_only = true;
        let original = linked_video_details("9bZkp7q19f0", "Original selection", "#One #Two");
        controller.youtube_results = vec![
            SearchItem::Video(summary_from_details(&linked_video_details(
                "dQw4w9WgXcQ",
                "First result",
                "#One",
            ))),
            SearchItem::Video(summary_from_details(&original)),
        ];
        let mut request = SearchRequest::new("original search", SearchTarget::Videos);
        request.page = 3;
        request.sort = ProviderSearchSort::UploadDate;
        request
            .filters
            .features
            .push(SearchFeature::CreativeCommons);
        controller.youtube_search_request = Some(request.clone());
        controller.next_youtube_page = Some(4);
        controller.refresh_youtube_rows();
        controller.view.selected = 1;
        controller.youtube_selected = 1;
        controller.view.details = Some(detail_from_video(&original, &controller.subscription_tree));
        controller.view.details_scroll = 17;
        controller.view.details_focused = true;
        controller.view.selected_detail_link = Some(1);
        controller.current_media = Some(MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ"));
        controller.view.playing_media_id = controller.current_media.clone();
        controller
            .store
            .save_youtube_search(
                &SavedYouTubeSearch {
                    request,
                    results: controller.youtube_results.clone(),
                    next_page: Some(4),
                },
                unix_time(),
            )
            .unwrap();
        (temporary, controller, requests)
    }

    /// Extracts the owned search while ignoring optional selected-row enrichment.
    fn take_search(requests: &Receiver<ProviderRequest>) -> (u64, SearchRequest) {
        requests
            .try_iter()
            .find_map(|request| match request {
                ProviderRequest::Search {
                    generation,
                    request,
                } => Some((generation, request)),
                _ => None,
            })
            .expect("captured search request")
    }

    /// Delivers deterministic provider results and leaves later metadata requests captured.
    fn accept_search(controller: &mut AppController, generation: u64, request: SearchRequest) {
        controller.handle_provider_response(ProviderResponse::Search {
            generation,
            result: Ok(SearchPage {
                page: request.page,
                items: vec![
                    SearchItem::Video(summary_from_details(&linked_video_details(
                        "M7lc1UVf-VE",
                        "Hashtag result",
                        "#Two",
                    ))),
                    SearchItem::Video(summary_from_details(&linked_video_details(
                        "aaaaaaaaaaa",
                        "Another hashtag result",
                        "#Three",
                    ))),
                ],
                next_page: Some(request.page + 1),
            }),
            request,
        });
    }

    /// Back restores accepted rows, exact selection, Details and restart-safe continuation.
    #[test]
    fn youtube_hashtag_back_restores_search_and_persisted_pagination() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        let results = controller.youtube_results.clone();
        let saved = controller.store.youtube_search().unwrap();
        let playing = controller.current_media.clone();
        let queue = controller.playback_queue.clone();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let (generation, request) = take_search(&requests);
        accept_search(&mut controller, generation, request);
        controller.dispatch(UiAction::SelectRow(1));
        let _ = requests.try_iter().count();

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.search_query, "original search");
        assert_eq!(controller.youtube_search_query, "original search");
        assert_eq!(controller.youtube_results, results);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.youtube_selected, 1);
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Original selection"
        );
        assert_eq!(controller.view.details_scroll, 17);
        assert!(controller.view.details_focused);
        assert_eq!(controller.view.selected_detail_link, Some(1));
        assert_eq!(
            controller.view.youtube_search_sort,
            YouTubeSearchSort::Newest
        );
        assert!(controller.view.youtube_creative_commons_only);
        assert_eq!(controller.next_youtube_page, Some(4));
        assert_eq!(controller.store.youtube_search().unwrap(), saved);
        assert_eq!(controller.current_media, playing);
        assert_eq!(controller.playback_queue, queue);
        assert!(
            !requests
                .try_iter()
                .any(|request| matches!(request, ProviderRequest::Search { .. }))
        );
        controller.submit_youtube_search(4);
        let (_, next) = take_search(&requests);
        assert_eq!(next.query, "original search");
        assert_eq!(next.page, 4);
        assert_eq!(next.sort, ProviderSearchSort::UploadDate);
        assert!(
            next.filters
                .features
                .contains(&SearchFeature::CreativeCommons)
        );
    }

    /// A pending hashtag can be left immediately, revoking both results and errors.
    #[test]
    fn youtube_hashtag_back_revokes_pending_responses() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let (generation, request) = take_search(&requests);
        let details_generation = controller.details_generation;
        controller.dispatch(UiAction::GoBack);
        accept_search(&mut controller, generation, request.clone());
        controller.handle_provider_response(ProviderResponse::Search {
            generation,
            request,
            result: Err("stale hashtag failure".into()),
        });
        controller.handle_provider_response(ProviderResponse::Details {
            generation: details_generation,
            result: Ok(linked_video_details(
                "M7lc1UVf-VE",
                "Stale details",
                "#Stale",
            )),
        });
        assert_eq!(controller.view.search_query, "original search");
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Original selection"
        );
        assert!(controller.view.search_activity.is_none());
        assert!(controller.view.error_popup.is_none());
    }

    /// Nested hashtag searches preserve each accepted result set and its reading position.
    #[test]
    fn youtube_hashtag_back_restores_chained_searches() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let (generation, request) = take_search(&requests);
        accept_search(&mut controller, generation, request);
        controller.view.details_scroll = 6;
        controller.dispatch(UiAction::SearchYouTubeHashtag("Two".into()));
        let (generation, request) = take_search(&requests);
        accept_search(&mut controller, generation, request);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.search_query, "#One");
        assert_eq!(controller.view.details_scroll, 6);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.search_query, "original search");
        assert_eq!(controller.view.details_scroll, 17);
    }

    /// Returning to another tab restores its query, row and subscription navigation context.
    #[test]
    fn youtube_hashtag_back_restores_cross_tab_origin() {
        for screen in [
            Screen::Subscriptions,
            Screen::History,
            Screen::Playlists,
            Screen::YouTubeMusic,
        ] {
            let (_temporary, mut controller, requests) = hashtag_controller();
            controller.view.screen = screen;
            controller.view.search_query = "origin tab query".into();
            controller.view.search_cursor_byte = controller.view.search_query.len();
            controller.view.subscriptions.route = SubscriptionRoute::Items;
            controller.view.subscriptions.focus = SubscriptionPane::Items;
            controller.view.subscriptions.selected_source = 2;
            controller.view.subscriptions.selected_item = 1;
            controller.view.subscriptions.description_expanded = true;
            controller.active_subscription_channel_id = Some("UCfixture".into());
            controller.subscription_video_cache.insert(
                "UCfixture".into(),
                CachedSubscriptionVideos {
                    items: controller.youtube_results.clone(),
                    next_page: Some(4),
                    ..CachedSubscriptionVideos::default()
                },
            );
            controller.view.subscriptions.items = controller.view.rows.clone();
            controller.youtube_music_results = controller.youtube_results.clone();
            controller.youtube_music_search_query = "origin tab query".into();
            controller.youtube_music_selected = 1;
            controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
            let (generation, request) = take_search(&requests);
            accept_search(&mut controller, generation, request);
            controller.dispatch(UiAction::GoBack);
            assert_eq!(controller.view.screen, screen);
            assert_eq!(controller.view.search_query, "origin tab query");
            assert_eq!(controller.youtube_search_query, "original search");
            assert_eq!(controller.view.selected, 1);
            assert_eq!(controller.view.rows[1].title, "Original selection");
            assert_eq!(controller.view.details_scroll, 17);
            assert_eq!(
                controller.view.subscriptions.route,
                SubscriptionRoute::Items
            );
            assert_eq!(controller.view.subscriptions.focus, SubscriptionPane::Items);
            assert_eq!(controller.view.subscriptions.selected_source, 2);
            assert_eq!(controller.view.subscriptions.selected_item, 1);
            assert!(controller.view.subscriptions.description_expanded);
            assert_eq!(
                controller.active_subscription_channel_id.as_deref(),
                Some("UCfixture")
            );
            if matches!(screen, Screen::Subscriptions | Screen::YouTubeMusic) {
                assert_eq!(
                    controller
                        .selected_youtube_item()
                        .and_then(subscription_item_media_id),
                    Some(MediaId::new(SourceKind::YouTube, "9bZkp7q19f0"))
                );
                assert_eq!(
                    controller.selected_queue_item().unwrap().media.id,
                    MediaId::new(SourceKind::YouTube, "9bZkp7q19f0")
                );
            }
        }
    }

    /// Returning from a hashtag must retain the earlier description-link Back chain.
    #[test]
    fn youtube_hashtag_back_preserves_description_navigation() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.dispatch(UiAction::ActivateDescriptionVideo {
            video_id: "M7lc1UVf-VE".into(),
            start_seconds: None,
        });
        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(linked_video_details("M7lc1UVf-VE", "Linked origin", "#One")),
        });
        controller.view.details_scroll = 9;
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let (generation, request) = take_search(&requests);
        accept_search(&mut controller, generation, request);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Linked origin"
        );
        assert_eq!(controller.view.details_scroll, 9);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Original selection"
        );
        assert_eq!(controller.view.details_scroll, 17);
    }

    /// Hashtag navigation clears text-selection mode until the original Details returns.
    #[test]
    fn youtube_hashtag_back_restores_text_selection_only_at_origin() {
        let (_temporary, mut controller, _requests) = hashtag_controller();
        controller.view.text_selection_mode = true;
        let selection = DetailsTextSelection {
            dragging: true,
            ..DetailsTextSelection::default()
        };
        controller.view.details_text_selection = Some(selection);
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        assert!(!controller.view.text_selection_mode);
        assert!(controller.view.details_text_selection.is_none());
        controller.dispatch(UiAction::GoBack);
        assert!(controller.view.text_selection_mode);
        assert_eq!(controller.view.details_text_selection, Some(selection));
    }

    /// An explicit query or direct URL owns a fresh route and abandons hashtag Back.
    #[test]
    fn youtube_hashtag_back_does_not_restore_after_independent_input() {
        for query in ["independent query", "https://youtu.be/M7lc1UVf-VE"] {
            let (_temporary, mut controller, requests) = hashtag_controller();
            controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
            let (generation, request) = take_search(&requests);
            accept_search(&mut controller, generation, request);
            controller.view.search_query = query.into();
            controller.dispatch(UiAction::SubmitSearch);
            assert!(controller.youtube_hashtag_history.is_empty());
            controller.dispatch(UiAction::GoBack);
            assert_eq!(controller.view.search_query, query);
        }
        let (_temporary, mut controller, _requests) = hashtag_controller();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        controller.dispatch(UiAction::ShowScreen(Screen::Playlists));
        assert!(controller.youtube_hashtag_history.is_empty());
    }

    /// Each retained location has a fixed lifetime and old chains cannot grow without bound.
    #[test]
    fn youtube_hashtag_back_history_is_bounded() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        for index in 0..MAX_HASHTAG_LOCATIONS + 3 {
            controller.dispatch(UiAction::SearchYouTubeHashtag(format!("Tag{index}")));
            let (generation, request) = take_search(&requests);
            accept_search(&mut controller, generation, request);
        }
        assert_eq!(
            controller.youtube_hashtag_history.len(),
            MAX_HASHTAG_LOCATIONS
        );
        for _ in 0..MAX_HASHTAG_LOCATIONS {
            controller.dispatch(UiAction::GoBack);
        }
        assert!(controller.youtube_hashtag_history.is_empty());
        assert_eq!(controller.view.search_query, "#Tag2");
    }

    /// Paging and accepted enrichment keep the cached return origin and reading position.
    #[test]
    fn youtube_hashtag_back_survives_pagination_and_metadata_enrichment() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let (generation, request) = take_search(&requests);
        accept_search(&mut controller, generation, request);
        controller.submit_youtube_search(2);
        let (generation, request) = take_search(&requests);
        accept_search(&mut controller, generation, request);
        controller.dispatch(UiAction::GoBack);
        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(linked_video_details(
                "9bZkp7q19f0",
                "Enriched original",
                "#One #Two",
            )),
        });
        assert_eq!(controller.view.search_query, "original search");
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Enriched original"
        );
        assert_eq!(controller.view.details_scroll, 17);
        assert_eq!(controller.view.selected, 1);
    }

    /// Optional channel statistics cannot project YouTube results over another adapter.
    #[test]
    fn youtube_hashtag_back_late_channel_counts_preserve_origin_rows() {
        for direct in [false, true] {
            let (_temporary, mut controller, requests) = hashtag_controller();
            if direct {
                controller.direct_item = Some(DirectSourceInput {
                    source: SourceKind::RemoteFiles,
                    url: url::Url::parse("https://audio.example.test/origin.mp3").unwrap(),
                });
            } else {
                controller.view.screen = Screen::YouTubeMusic;
            }
            controller.view.rows = vec![RowView {
                title: "Visible non-search origin".into(),
                ..RowView::default()
            }];
            controller.view.selected = 0;
            controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
            let (generation, request) = take_search(&requests);
            accept_search(&mut controller, generation, request);
            controller.dispatch(UiAction::GoBack);
            controller.handle_provider_response(ProviderResponse::ChannelSubscriberCounts {
                provider_generation: controller.youtube_provider_generation,
                requested_ids: vec!["UCfixture".into()],
                result: Ok(vec![ChannelSubscriberCount {
                    channel_id: "UCfixture".into(),
                    subscriber_count: Some(42),
                    webpage_url: None,
                }]),
            });
            assert_eq!(controller.view.rows.len(), 1);
            assert_eq!(controller.view.rows[0].title, "Visible non-search origin");
            assert_eq!(controller.view.details_scroll, 17);
        }
    }

    /// Retaining a source protects exact Back state while its live cache still serves autoplay.
    #[test]
    fn youtube_hashtag_back_preserves_subscription_cache_and_autoplay() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.view.screen = Screen::Subscriptions;
        controller.view.subscriptions.route = SubscriptionRoute::Items;
        controller.view.subscriptions.focus = SubscriptionPane::Items;
        controller.view.subscriptions.items = controller.view.rows.clone();
        controller.view.subscriptions.selected_item = 1;
        controller.active_subscription_channel_id = Some("UCfixture".into());
        controller.subscription_video_cache.insert(
            "UCfixture".into(),
            CachedSubscriptionVideos {
                items: controller.youtube_results.clone(),
                next_page: Some(4),
                ..CachedSubscriptionVideos::default()
            },
        );
        let old_generation = controller.subscription_generation;
        controller.view.subscriptions.loading = true;
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let (generation, request) = take_search(&requests);
        accept_search(&mut controller, generation, request);
        assert!(matches!(
            controller.next_autoplay_step(&AutoplayOrigin::Subscription {
                channel_id: "UCfixture".into(),
                index: 0,
            }),
            AutoplayStep::Play { .. }
        ));
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation: old_generation,
            request: ChannelVideosRequest::new("UCfixture"),
            result: Ok(SearchPage {
                page: 1,
                items: vec![],
                next_page: None,
            }),
        });
        assert_eq!(
            controller.subscription_video_cache["UCfixture"].items.len(),
            2
        );
        // Inactive caches may be evicted while another page is visible; the
        // retained origin must remain playable when returned to.
        controller.subscription_video_cache.remove("UCfixture");
        controller.dispatch(UiAction::GoBack);
        assert_eq!(
            controller
                .selected_subscription_item()
                .and_then(subscription_item_media_id),
            Some(MediaId::new(SourceKind::YouTube, "9bZkp7q19f0"))
        );
        assert_eq!(
            controller.selected_queue_item().unwrap().media.id,
            MediaId::new(SourceKind::YouTube, "9bZkp7q19f0")
        );
        assert_eq!(
            controller.subscription_video_cache["UCfixture"].next_page,
            Some(4)
        );
        assert!(!controller.view.subscriptions.loading);
    }

    /// Invalid input and unavailable setup must not displace the current origin.
    #[test]
    fn youtube_hashtag_back_rejects_unstarted_navigation() {
        let (_temporary, mut controller, _requests) = hashtag_controller();
        controller.dispatch(UiAction::SearchYouTubeHashtag("two words".into()));
        assert!(controller.youtube_hashtag_history.is_empty());
        controller.youtube_provider_available = false;
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        assert!(controller.youtube_hashtag_history.is_empty());
        assert_eq!(controller.view.search_query, "original search");
    }

    /// Back reapplies the current Shorts preference while retaining a surviving selection.
    #[test]
    fn youtube_hashtag_back_reprojects_shorts_without_losing_selected_identity() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.config.ui.show_youtube_shorts = true;
        let SearchItem::Video(short) = &mut controller.youtube_results[0] else {
            panic!("fixture must contain a video");
        };
        short.orientation = VideoOrientation::Vertical;
        controller.refresh_youtube_rows();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let _ = requests.try_iter().count();
        controller.config.ui.show_youtube_shorts = false;

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.rows.len(), 1);
        assert_eq!(controller.view.rows[0].title, "Original selection");
        assert_eq!(controller.view.selected, 0);
        assert_eq!(controller.youtube_selected, 0);
        assert_eq!(controller.view.details_scroll, 17);
        assert_eq!(controller.view.selected_detail_link, Some(1));
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Original selection"
        );
        assert_eq!(
            controller.selected_queue_item().unwrap().media.id,
            MediaId::new(SourceKind::YouTube, "9bZkp7q19f0")
        );
        assert_eq!(controller.youtube_results.len(), 2);
        assert!(!requests.try_iter().any(|request| matches!(
            request,
            ProviderRequest::Search { .. } | ProviderRequest::Details { .. }
        )));
    }

    /// A restored Short loses Details ownership when the shared preference hides its row.
    #[test]
    fn youtube_hashtag_back_replaces_hidden_short_details_without_requesting_metadata() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.config.ui.show_youtube_shorts = true;
        let SearchItem::Video(short) = &mut controller.youtube_results[1] else {
            panic!("fixture must contain a video");
        };
        short.orientation = VideoOrientation::Vertical;
        controller.refresh_youtube_rows();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let _ = requests.try_iter().count();
        controller.config.ui.show_youtube_shorts = false;

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.rows.len(), 1);
        assert_eq!(controller.view.rows[0].title, "First result");
        assert_eq!(controller.view.selected, 0);
        assert_eq!(controller.youtube_selected, 0);
        assert_eq!(controller.view.details_scroll, 0);
        assert_eq!(controller.view.selected_detail_link, None);
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "First result"
        );
        assert_eq!(
            controller.selected_queue_item().unwrap().media.id,
            MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")
        );
        assert!(!requests.try_iter().any(|request| matches!(
            request,
            ProviderRequest::Search { .. } | ProviderRequest::Details { .. }
        )));
    }

    /// Description history keeps its row identity when filtering moves the visible offset.
    #[test]
    fn youtube_hashtag_back_rebases_description_history_after_shorts_change() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.config.ui.show_youtube_shorts = true;
        let SearchItem::Video(short) = &mut controller.youtube_results[0] else {
            panic!("fixture must contain a video");
        };
        short.orientation = VideoOrientation::Vertical;
        controller.refresh_youtube_rows();
        controller.dispatch(UiAction::ActivateDescriptionVideo {
            video_id: "M7lc1UVf-VE".into(),
            start_seconds: None,
        });
        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(linked_video_details("M7lc1UVf-VE", "Linked origin", "#One")),
        });
        controller.view.details_scroll = 9;
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let _ = requests.try_iter().count();
        controller.config.ui.show_youtube_shorts = false;

        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.rows.len(), 1);
        assert_eq!(controller.view.selected, 0);
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Linked origin"
        );
        assert_eq!(controller.view.details_scroll, 9);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.selected, 0);
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Original selection"
        );
        assert_eq!(controller.view.details_scroll, 17);
        assert_eq!(
            controller.selected_queue_item().unwrap().media.id,
            MediaId::new(SourceKind::YouTube, "9bZkp7q19f0")
        );
    }

    /// Other tabs keep their rows while the parked search adapter selection is rebased.
    #[test]
    fn youtube_hashtag_back_reprojects_hidden_search_adapter_selection() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.config.ui.show_youtube_shorts = true;
        let SearchItem::Video(short) = &mut controller.youtube_results[0] else {
            panic!("fixture must contain a video");
        };
        short.orientation = VideoOrientation::Vertical;
        controller.view.screen = Screen::YouTubeMusic;
        controller.youtube_music_results = controller.youtube_results.clone();
        controller.view.search_query = "music origin".into();
        controller.refresh_youtube_music_rows();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let _ = requests.try_iter().count();
        controller.config.ui.show_youtube_shorts = false;

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.screen, Screen::YouTubeMusic);
        assert_eq!(controller.view.rows.len(), 2);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.view.search_query, "music origin");
        assert_eq!(controller.youtube_selected, 0);
        assert_eq!(controller.view.details_scroll, 17);
        assert_eq!(
            controller.selected_queue_item().unwrap().media.id,
            MediaId::new(SourceKind::YouTube, "9bZkp7q19f0")
        );
    }

    /// Reenabling Shorts restores rows without moving the selection to the newly shown video.
    #[test]
    fn youtube_hashtag_back_reprojects_when_shorts_are_reenabled() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        let SearchItem::Video(short) = &mut controller.youtube_results[0] else {
            panic!("fixture must contain a video");
        };
        short.orientation = VideoOrientation::Vertical;
        controller.config.ui.show_youtube_shorts = false;
        controller.refresh_youtube_rows();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let _ = requests.try_iter().count();
        controller.config.ui.show_youtube_shorts = true;

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.rows.len(), 2);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.youtube_selected, 1);
        assert_eq!(controller.view.details_scroll, 17);
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Original selection"
        );
        assert_eq!(
            controller.selected_queue_item().unwrap().media.id,
            MediaId::new(SourceKind::YouTube, "9bZkp7q19f0")
        );
    }

    /// Returning to a subscription uses the same new preference as the visible hashtag page.
    #[test]
    fn youtube_hashtag_back_reprojects_subscription_origin_shorts() {
        for short_index in [0, 1] {
            let (_temporary, mut controller, requests) = hashtag_controller();
            controller.config.ui.show_youtube_shorts = true;
            let SearchItem::Video(short) = &mut controller.youtube_results[short_index] else {
                panic!("fixture must contain a video");
            };
            short.orientation = VideoOrientation::Vertical;
            controller.view.screen = Screen::Subscriptions;
            controller.view.subscriptions.route = SubscriptionRoute::Items;
            controller.view.subscriptions.focus = SubscriptionPane::Items;
            controller.view.subscriptions.selected_item = 1;
            controller.active_subscription_channel_id = Some("UCfixture".into());
            controller.subscription_video_cache.insert(
                "UCfixture".into(),
                CachedSubscriptionVideos {
                    items: controller.youtube_results.clone(),
                    next_page: Some(4),
                    ..CachedSubscriptionVideos::default()
                },
            );
            controller.refresh_subscription_video_rows();
            controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
            let _ = requests.try_iter().count();
            controller.config.ui.show_youtube_shorts = false;

            controller.dispatch(UiAction::GoBack);

            assert_eq!(controller.view.screen, Screen::Subscriptions);
            assert_eq!(controller.view.subscriptions.items.len(), 1);
            assert_eq!(controller.view.subscriptions.selected_item, 0);
            let (title, id, scroll) = if short_index == 0 {
                ("Original selection", "9bZkp7q19f0", 17)
            } else {
                ("First result", "dQw4w9WgXcQ", 0)
            };
            assert_eq!(controller.view.subscriptions.items[0].title, title);
            assert_eq!(controller.view.details.as_ref().unwrap().title, title);
            assert_eq!(controller.view.details_scroll, scroll);
            assert_eq!(
                controller.selected_queue_item().unwrap().media.id,
                MediaId::new(SourceKind::YouTube, id)
            );
            assert!(!requests.try_iter().any(|request| matches!(
                request,
                ProviderRequest::Search { .. } | ProviderRequest::Details { .. }
            )));
        }
    }

    /// A hidden cached item must not replace the channel description owned by Sources focus.
    #[test]
    fn youtube_hashtag_back_shorts_change_preserves_subscription_source_details() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.config.ui.show_youtube_shorts = true;
        let SearchItem::Video(short) = &mut controller.youtube_results[1] else {
            panic!("fixture must contain a video");
        };
        short.orientation = VideoOrientation::Vertical;
        controller.view.screen = Screen::Subscriptions;
        controller.view.subscriptions.route = SubscriptionRoute::Sources;
        controller.view.subscriptions.focus = SubscriptionPane::Sources;
        controller.view.subscriptions.selected_item = 1;
        controller.active_subscription_channel_id = Some("UCfixture".into());
        controller.subscription_video_cache.insert(
            "UCfixture".into(),
            CachedSubscriptionVideos {
                items: controller.youtube_results.clone(),
                ..CachedSubscriptionVideos::default()
            },
        );
        controller.refresh_subscription_video_rows();
        controller.view.details.as_mut().unwrap().title = "Channel description".into();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let _ = requests.try_iter().count();
        controller.config.ui.show_youtube_shorts = false;

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.subscriptions.items.len(), 1);
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "Channel description"
        );
        assert_eq!(controller.view.details_scroll, 17);
    }

    /// Exact video URLs remain reachable when Back restores a vertical direct result.
    #[test]
    fn youtube_hashtag_back_keeps_direct_short_visible() {
        let (_temporary, mut controller, requests) = hashtag_controller();
        controller.youtube_search_request = None;
        controller.youtube_results.remove(0);
        let SearchItem::Video(short) = &mut controller.youtube_results[0] else {
            panic!("fixture must contain a video");
        };
        short.orientation = VideoOrientation::Vertical;
        controller.config.ui.show_youtube_shorts = true;
        controller.refresh_youtube_rows();
        controller.dispatch(UiAction::SearchYouTubeHashtag("One".into()));
        let _ = requests.try_iter().count();
        controller.config.ui.show_youtube_shorts = false;

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.rows.len(), 1);
        assert_eq!(controller.view.rows[0].title, "Original selection");
        assert_eq!(controller.view.details_scroll, 17);
        assert!(controller.youtube_search_request.is_none());
        assert_eq!(
            controller.selected_queue_item().unwrap().media.id,
            MediaId::new(SourceKind::YouTube, "9bZkp7q19f0")
        );
        assert!(!requests.try_iter().any(|request| matches!(
            request,
            ProviderRequest::Search { .. } | ProviderRequest::Details { .. }
        )));
    }
}
