//! Temporary `YouTube` channel browsing through the existing subscription panes.

use super::*;

/// Bounds nested channel visits reached through description hashtag searches.
const MAX_CHANNEL_LOCATIONS: usize = 16;

/// Recognizes a whole handle or official handle URL without decoding it twice.
///
/// Embedded mentions remain ordinary search text. The provider owns bounded
/// handle validation and percent decoding; URL parsing here only classifies
/// known YouTube hosts and preserves their raw encoded reference.
pub(super) fn parse_handle_input(raw: &str) -> Option<String> {
    let input = raw.trim();
    if input.is_empty() || input.chars().any(char::is_whitespace) {
        return None;
    }
    if input.starts_with('@') {
        return Some(input.to_owned());
    }
    let normalized = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let url = url::Url::parse(&normalized).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(
            url.host_str()?,
            "youtube.com"
                | "www.youtube.com"
                | "m.youtube.com"
                | "music.youtube.com"
                | "youtube-nocookie.com"
                | "www.youtube-nocookie.com"
        )
    {
        return None;
    }
    // Description-link limits count encoded bytes. A Unicode handle URL can
    // exceed those limits while remaining valid after the provider decodes it.
    let reference = url.path_segments()?.find(|segment| !segment.is_empty())?;
    reference.starts_with('@').then(|| reference.to_owned())
}

/// A transient source and the search and subscription views that it displaced.
pub(super) struct ChannelLocation {
    /// Search adapter and Details moved out while the channel is visible.
    origin: youtube_hashtag::HashtagLocation,
    /// Ordinary Subs view, or the parent transient channel during nested browsing.
    pub(super) subscriptions: crate::view::SubscriptionsView,
    /// Source entries backing the displaced Subs view.
    entries: Vec<FlattenedSubscription>,
    /// Renderer ownership retained with the displaced source rows.
    source_owner: Option<String>,
    /// One process-local channel entry that is never saved implicitly to OPML.
    pub(super) channel: FlattenedSubscription,
}

impl AppController {
    /// Resolves one handle through the configured provider without a text search.
    pub(super) fn submit_youtube_channel_handle(&mut self, handle: String) {
        if !self.youtube_provider_available {
            self.open_youtube_setup();
            return;
        }
        self.supersede_search_generation();
        let generation = self.search_generation;
        let provider_generation = self.youtube_provider_generation;
        if self.send_provider_request(
            ProviderRequest::ResolveYouTubeChannel {
                generation,
                provider_generation,
                handle,
            },
            "Could not resolve the YouTube channel",
        ) {
            self.pending_youtube_channel_handle = Some((generation, provider_generation));
            self.begin_search_activity(SearchActivity::YouTube);
            self.view.status_line = "Resolving YouTube channel...".into();
        }
    }

    /// Cancels an unaccepted lookup before a tab, editor or Back takes ownership.
    pub(super) fn cancel_youtube_channel_lookup(&mut self) -> bool {
        if self.pending_youtube_channel_handle.take().is_none() {
            return false;
        }
        self.supersede_search_generation();
        self.view
            .search_query
            .clone_from(&self.youtube_search_query);
        self.view.search_cursor_byte = self.view.search_query.len();
        true
    }

    /// Accepts metadata only for the latest visible lookup and current provider.
    pub(super) fn accept_youtube_channel_handle(
        &mut self,
        generation: u64,
        provider_generation: u64,
        result: Result<ChannelDetails, String>,
    ) {
        if self.pending_youtube_channel_handle != Some((generation, provider_generation))
            || generation != self.search_generation
            || provider_generation != self.youtube_provider_generation
            || self.view.screen != Screen::Search
        {
            return;
        }
        self.pending_youtube_channel_handle = None;
        self.finish_search_activity(SearchActivity::YouTube);
        let mut profile = match result {
            Ok(profile) => profile,
            Err(error) => {
                self.show_error_message("YouTube channel lookup failed", error);
                return;
            }
        };
        compact_channel_summary(&mut profile.summary);
        // The new editor text names the destination, not the results parked
        // behind it. Back must restore the last accepted search and its rows.
        self.view
            .search_query
            .clone_from(&self.youtube_search_query);
        self.view.search_cursor_byte = self.view.search_query.len();
        let channel_id = profile.summary.channel_id.clone();
        if !self.open_youtube_channel(
            channel_id.clone(),
            profile.summary.name.clone(),
            profile.summary.webpage_url.clone(),
        ) {
            return;
        }
        // Feed the complete resolver result through existing bounded metadata
        // caches and projections; do not issue another channel-details fetch.
        self.scheduled_channel_details = None;
        self.handle_provider_response(ProviderResponse::ChannelDetails {
            generation: self.channel_details_generation,
            provider_generation,
            channel_id,
            result: Ok(profile),
        });
        self.view.status_line =
            "Press Enter to browse channel videos; Alt+Left returns to search".into();
    }

    /// Opens the selected video's channel without adding a local subscription.
    pub(super) fn browse_youtube_channel(&mut self) {
        if !self.view.youtube_channel_browse_available() {
            self.view.status_line = "Select a YouTube video with a known channel".into();
            return;
        }
        let details = self.view.details.as_ref().expect("eligible video details");
        self.open_youtube_channel(
            details.channel_id.clone(),
            details.channel_name.clone(),
            details.channel_webpage_url.clone(),
        );
    }

    /// Shares the transient channel route between video owners and resolved handles.
    fn open_youtube_channel(
        &mut self,
        channel_id: String,
        name: String,
        webpage_url: Option<url::Url>,
    ) -> bool {
        if self.youtube_channel_navigation.len() >= MAX_CHANNEL_LOCATIONS {
            self.view.status_line = "Return to an earlier channel before opening another".into();
            return false;
        }
        let Some(url) = canonical_youtube_channel_url(&channel_id) else {
            self.view.status_line = "The video provider returned an invalid channel ID".into();
            return false;
        };
        let name = if name.is_empty() {
            channel_id.clone()
        } else {
            name
        };
        let mut subscription = subscriptions::Subscription::new(name, url);
        subscription.website_url = youtube_channel_webpage_url(&channel_id, webpage_url);
        subscription.auto_download = self
            .subscription_tree
            .youtube_channel_auto_download(&channel_id);
        let channel = FlattenedSubscription {
            depth: 0,
            subscription,
        };
        let location = ChannelLocation {
            origin: self.take_youtube_hashtag_location(),
            subscriptions: std::mem::take(&mut self.view.subscriptions),
            entries: std::mem::take(&mut self.subscription_entries),
            source_owner: self.subscription_source_owner_id.take(),
            channel: channel.clone(),
        };
        let source_generation = location.subscriptions.source_generation.wrapping_add(1);
        self.youtube_channel_navigation.push(location);
        self.view.youtube_channel_browsing = true;
        self.prepare_screen_transition(Screen::Subscriptions);
        self.supersede_search_generation();
        self.cancel_hashtag_subscription_work();
        self.view.screen = Screen::Subscriptions;
        self.view.selected = 0;
        self.view.search_query.clear();
        self.view.search_cursor_byte = 0;
        self.view.details_focused = false;
        self.view.details_scroll = 0;
        self.view.selected_detail_link = None;
        self.view.detail_link_reveal = None;
        self.view.text_selection_mode = false;
        self.view.details_text_selection = None;
        self.view.subscriptions.layout = SubscriptionsLayout::DrillDown;
        self.view.subscriptions.source_generation = source_generation;
        self.view.subscriptions.show_youtube_shorts = self.config.ui.show_youtube_shorts;
        self.subscription_entries = vec![channel];
        self.rebuild_subscription_source_rows();
        self.restore_cached_subscription_source_metadata();
        self.update_selected_subscription_source();
        self.refresh_selected_playlist_state();
        self.view.status_line =
            "Press Enter to browse channel videos; Alt+Left returns to the video".into();
        true
    }

    /// Leaves videos for their source, then restores the search before playback seek history.
    pub(super) fn go_back_youtube_channel(&mut self) -> bool {
        if self.view.screen != Screen::Subscriptions || self.youtube_channel_navigation.is_empty() {
            return false;
        }
        if self.move_detail_navigation(false) {
            return true;
        }
        if self.view.subscriptions.route == SubscriptionRoute::Items
            || self.view.subscriptions.focus == SubscriptionPane::Items
        {
            self.pending_subscription_refresh = None;
            self.cancel_subscription_item_work_for_navigation();
            self.view.subscriptions.route = SubscriptionRoute::Sources;
            self.view.subscriptions.focus = SubscriptionPane::Sources;
            self.view.subscriptions.description_expanded = false;
            self.view.details_focused = false;
            self.view.selected_detail_link = None;
            self.update_selected_subscription_source();
            self.refresh_selected_playlist_state();
            self.view.status_line = "Returned to channel information".into();
            return true;
        }
        let location = self
            .youtube_channel_navigation
            .pop()
            .expect("channel return location");
        self.restore_youtube_channel_origin(location);
        true
    }

    /// Restores moved search state while reflecting any explicit subscription changes.
    fn restore_youtube_channel_origin(&mut self, location: ChannelLocation) {
        self.view.youtube_channel_browsing = !self.youtube_channel_navigation.is_empty();
        self.view.subscriptions = location.subscriptions;
        self.view.subscriptions.show_youtube_shorts = self.config.ui.show_youtube_shorts;
        self.subscription_entries = location.entries;
        self.subscription_source_owner_id = location.source_owner;
        if !self.restore_youtube_location(location.origin) {
            // A provider replacement revokes the snapshot before the shared
            // restorer touches request owners. Revoke the departed live page
            // here too, including any nested transient parent sources.
            self.prepare_screen_transition(Screen::Search);
            self.supersede_search_generation();
            if !self.youtube_channel_navigation.is_empty() {
                let oldest = self.youtube_channel_navigation.remove(0);
                self.view.subscriptions = oldest.subscriptions;
                self.subscription_entries = oldest.entries;
                self.subscription_source_owner_id = oldest.source_owner;
                self.youtube_channel_navigation.clear();
            }
            self.view.youtube_channel_browsing = false;
            self.cancel_hashtag_subscription_work();
            self.youtube_results.clear();
            self.youtube_search_request = None;
            self.next_youtube_page = None;
            self.youtube_selected = 0;
            self.direct_item = None;
            self.resolved_direct = None;
            self.local_results.clear();
            self.selected_start_override = None;
            self.previous_detail = None;
            self.view.screen = Screen::Search;
            self.view
                .search_query
                .clone_from(&self.youtube_search_query);
            self.view.search_cursor_byte = self.view.search_query.len();
            self.view.selected = 0;
            self.view.right_panel_mode = RightPanelMode::Details;
            self.view.details = None;
            self.view.details_scroll = 0;
            self.view.details_focused = false;
            self.view.selected_detail_link = None;
            self.view.detail_link_reveal = None;
            self.view.text_selection_mode = false;
            self.view.details_text_selection = None;
            self.view.rows.clear();
            self.refresh_selected_playlist_state();
            self.view.status_line =
                "The YouTube provider changed; search again to load videos".into();
            return;
        }
        let tree = &self.subscription_tree;
        for detail in self
            .view
            .details
            .iter_mut()
            .chain(self.previous_detail.iter_mut())
            .chain(
                self.detail_navigation_back
                    .iter_mut()
                    .chain(self.detail_navigation_forward.iter_mut())
                    .flat_map(|snapshot| {
                        snapshot
                            .details
                            .iter_mut()
                            .chain(snapshot.previous_detail.iter_mut())
                    }),
            )
        {
            if !detail.channel_id.is_empty() {
                detail.channel_subscribed = tree.contains_youtube_channel(&detail.channel_id);
                detail.channel_auto_download =
                    tree.youtube_channel_auto_download(&detail.channel_id);
            }
        }
        if self.view.screen == Screen::Search
            && self.direct_item.is_none()
            && self.resolved_direct.is_none()
            && self.local_results.is_empty()
        {
            self.refresh_youtube_rows();
        }
        self.view.status_line = "Returned from YouTube channel".into();
    }

    /// Explicit tab changes retain the current search and restore the normal Subs pane.
    pub(super) fn abandon_youtube_channel_navigation(&mut self) {
        // A linked video's channel may directly nest another channel, without
        // a hashtag search between them. Walk those adapter-only parents until
        // a real search owns the YouTube tab again.
        while let Some(location) = self.youtube_channel_navigation.pop() {
            if self.view.screen == Screen::Subscriptions {
                self.restore_youtube_channel_origin(location);
            } else {
                self.view.subscriptions = location.subscriptions;
                self.subscription_entries = location.entries;
                self.subscription_source_owner_id = location.source_owner;
            }
            if self.view.screen != Screen::Subscriptions {
                break;
            }
        }
        if !self.youtube_channel_navigation.is_empty() {
            let oldest = self.youtube_channel_navigation.remove(0);
            self.view.subscriptions = oldest.subscriptions;
            self.subscription_entries = oldest.entries;
            self.subscription_source_owner_id = oldest.source_owner;
            self.youtube_channel_navigation.clear();
        }
        self.view.youtube_channel_browsing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{controller_with_youtube_video_comments, linked_video_details};
    use super::*;

    /// Creates a selected second result with a persisted unrelated subscription.
    fn fixture() -> (tempfile::TempDir, AppController, Receiver<ProviderRequest>) {
        let (temporary, mut controller, requests) = controller_with_youtube_video_comments();
        controller.youtube_provider_available = true;
        let mut tree = SubscriptionTree::default();
        assert!(tree.subscribe_youtube_channel("Saved channel", "UCsaved"));
        subscriptions::save(&controller.config, &tree).unwrap();
        controller.subscription_tree = tree;
        controller
            .youtube_results
            .insert(0, controller.youtube_results[0].clone());
        controller.view.selected = 1;
        controller.youtube_selected = 1;
        controller.view.search_query = "original search".into();
        controller.youtube_search_query = controller.view.search_query.clone();
        controller.view.search_cursor_byte = 7;
        controller.youtube_search_request =
            Some(SearchRequest::new("original search", SearchTarget::Videos));
        controller.next_youtube_page = Some(3);
        controller.refresh_youtube_rows();
        controller.view.details_scroll = 17;
        controller.view.details_focused = true;
        controller.view.selected_detail_link = Some(2);
        (temporary, controller, requests)
    }

    /// A transient channel uses YouTube's top-level navigation and saved route.
    #[test]
    fn browse_youtube_channel_keeps_youtube_tab_navigation_and_session() {
        let (_temporary, mut controller, _requests) = fixture();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        for (key, expected) in [
            (crate::keymap::Key::Tab, Screen::Search.next_available(true)),
            (
                crate::keymap::Key::BackTab,
                Screen::Search.previous_available(true),
            ),
        ] {
            assert_eq!(
                crate::keymap::key_action(
                    crate::keymap::KeyPress::new(key),
                    &controller.view,
                    None,
                    None
                ),
                Some(UiAction::ShowScreen(expected)),
            );
        }
        assert!(controller.save_session());
        let saved = controller.store.session().unwrap().unwrap();
        assert_eq!(saved.screen, StoredScreen::Search);
        assert_eq!(saved.search_text, "original search");
        assert_eq!(saved.youtube_selected_row, Some(1));
    }

    /// Starting another query restores the normal YouTube editor, never the Subs editor.
    #[test]
    fn browse_youtube_channel_begin_search_returns_to_youtube() {
        let (_temporary, mut controller, _requests) = fixture();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::BeginSearch);
        assert_eq!(controller.view.screen, Screen::Search);
        assert!(controller.view.search_editing);
        assert_eq!(controller.view.search_query, "original search");
        assert!(controller.youtube_channel_navigation.is_empty());
        controller.dispatch(UiAction::CancelSearch);
        assert_eq!(controller.view.screen, Screen::Search);
        assert_eq!(controller.view.rows.len(), 2);
    }

    /// Clicking the displayed YouTube tab returns to its parked results.
    #[test]
    fn browse_youtube_channel_youtube_tab_restores_search() {
        let (_temporary, mut controller, _requests) = fixture();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::ShowScreen(Screen::Search));
        assert_eq!(controller.view.screen, Screen::Search);
        assert!(controller.youtube_channel_navigation.is_empty());
        assert_eq!(controller.view.search_query, "original search");
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.view.rows.len(), 2);
    }

    /// A linked video's channel can nest inside a channel without losing the parked search.
    #[test]
    fn browse_youtube_channel_child_back_and_tab_restore_their_owners() {
        for return_by_tab in [false, true] {
            let (_temporary, mut controller, _requests) = fixture();
            controller.dispatch(UiAction::BrowseYouTubeChannel);
            controller.view.details = Some(DetailView {
                media_id: Some(MediaId::new(SourceKind::YouTube, "linked-video")),
                channel_id: "UCchild".into(),
                channel_name: "Child channel".into(),
                ..DetailView::default()
            });
            assert!(controller.view.youtube_channel_browse_available());
            controller.dispatch(UiAction::BrowseYouTubeChannel);
            assert_eq!(controller.youtube_channel_navigation.len(), 2);
            assert_eq!(controller.view.displayed_screen(), Screen::Search);
            if !return_by_tab {
                controller.dispatch(UiAction::GoBack);
                assert_eq!(controller.youtube_channel_navigation.len(), 1);
                assert_eq!(
                    controller.view.subscriptions.sources[0].title,
                    "Fixture channel"
                );
                assert_eq!(controller.view.displayed_screen(), Screen::Search);
            }
            controller.dispatch(UiAction::ShowScreen(Screen::Search));
            assert_eq!(controller.view.rows.len(), 2);
            assert_eq!(controller.view.selected, 1);
            assert_eq!(controller.view.search_query, "original search");
            assert!(!controller.view.youtube_channel_browsing);
            controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
            assert_eq!(
                controller.view.subscriptions.sources[0].title,
                "Saved channel"
            );
        }
    }

    /// A whole handle is a direct channel lookup, not an ordinary video search.
    #[test]
    fn youtube_handle_search_does_not_issue_text_search() {
        for query in [
            "@fixture",
            "https://www.youtube.com/@fixture",
            "youtube.com/@fixture/videos",
        ] {
            let (_temporary, mut controller, requests) = fixture();
            controller.view.search_query = query.to_owned();
            controller.dispatch(UiAction::SubmitSearch);
            let sent = requests.try_iter().collect::<Vec<_>>();
            assert!(
                !sent
                    .iter()
                    .any(|request| matches!(request, ProviderRequest::Search { .. })),
                "{query}"
            );
            assert_eq!(
                sent.len(),
                1,
                "a handle must start one asynchronous channel lookup: {query}"
            );
            assert!(matches!(
                sent[0],
                ProviderRequest::ResolveYouTubeChannel { .. }
            ));
        }
    }

    /// Deterministic provider metadata for a handle, with no external requests.
    fn handle_profile() -> ChannelDetails {
        ChannelDetails {
            summary: ChannelSummary {
                channel_id: "UC_x5XG1OV2P6uZZ5FSM9Ttw".into(),
                name: "Handle channel".into(),
                description: "Resolved channel description".into(),
                subscriber_count: Some(42),
                video_count: Some(8),
                created_at: None,
                auto_generated: false,
                thumbnails: Vec::new(),
                webpage_url: Some(url::Url::parse("https://www.youtube.com/@fixture").unwrap()),
            },
            total_view_count: Some(100),
            country: Some("GB".into()),
            external_links: Vec::new(),
            external_links_truncated: false,
        }
    }

    /// Extracts the exact lookup owner from the mock worker lane.
    fn take_handle_request(requests: &Receiver<ProviderRequest>) -> (u64, u64, String) {
        requests
            .try_iter()
            .find_map(|request| match request {
                ProviderRequest::ResolveYouTubeChannel {
                    generation,
                    provider_generation,
                    handle,
                } => Some((generation, provider_generation, handle)),
                _ => None,
            })
            .expect("one handle lookup")
    }

    /// A resolved handle owns the same channel route and never creates a subscription.
    #[test]
    fn youtube_handle_lookup_opens_channel_and_back_restores_search() {
        let (_temporary, mut controller, requests) = fixture();
        let saved = std::fs::read(controller.config.subscriptions_file()).unwrap();
        controller.view.search_query = "@fixture".into();
        controller.dispatch(UiAction::SubmitSearch);
        let (generation, provider_generation, handle) = take_handle_request(&requests);
        assert_eq!(handle, "@fixture");
        assert!(controller.save_session());
        assert_eq!(
            controller.store.session().unwrap().unwrap().search_text,
            "original search",
            "saving a pending lookup must preserve the parked search"
        );
        controller.handle_provider_response(ProviderResponse::ResolvedYouTubeChannel {
            generation,
            provider_generation,
            result: Ok(handle_profile()),
        });
        assert_eq!(controller.view.displayed_screen(), Screen::Search);
        assert_eq!(controller.view.screen, Screen::Subscriptions);
        assert!(controller.view.youtube_channel_browsing);
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        assert_eq!(
            controller.view.subscriptions.sources[0].title,
            "Handle channel"
        );
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            "Resolved channel description"
        );
        assert_eq!(
            controller.view.details.as_ref().unwrap().channel_country,
            "GB"
        );
        assert!(!controller.view.details.as_ref().unwrap().channel_subscribed);
        controller.request_due_channel_details(Instant::now() + Duration::from_secs(1));
        assert!(
            !requests
                .try_iter()
                .any(|request| matches!(request, ProviderRequest::ChannelDetails { .. }))
        );
        controller.dispatch(UiAction::ActivateSelection);
        assert!(
            requests
                .try_iter()
                .any(|request| matches!(request, ProviderRequest::ChannelVideos { .. }))
        );
        controller.dispatch(UiAction::GoBack);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Search);
        assert!(!controller.view.youtube_channel_browsing);
        assert_eq!(controller.view.search_query, "original search");
        assert_eq!(controller.view.rows.len(), 2);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(
            std::fs::read(controller.config.subscriptions_file()).unwrap(),
            saved
        );
    }

    /// Only a whole channel token resolves; the adapter receives percent encoding unchanged.
    #[test]
    fn youtube_handle_parser_preserves_encoding_and_ordinary_queries() {
        for (input, expected) in [
            (" @fixture ", Some("@fixture")),
            (
                "https://www.youtube.com/@%D0%BA%D0%B0%D0%BD%D0%B0%D0%BB/videos",
                Some("@%D0%BA%D0%B0%D0%BD%D0%B0%D0%BB"),
            ),
            ("music @fixture", None),
            ("@fixture live", None),
            ("https://youtube.com.evil.test/@fixture", None),
            ("https://user:secret@youtube.com/@fixture", None),
            ("ftp://youtube.com/@fixture", None),
            ("https://youtube.com/watch?v=dQw4w9WgXcQ", None),
        ] {
            assert_eq!(parse_handle_input(input).as_deref(), expected, "{input}");
        }
        let encoded_handle = format!("@{}", "%D0%BA".repeat(30));
        assert_eq!(
            parse_handle_input(&format!("https://youtube.com/{encoded_handle}/videos")),
            Some(encoded_handle),
            "the provider validates decoded Unicode length, not the URL's encoded length"
        );
        for reference in ["@fixture%2Fevil", "@fixture%252Fevil", "@fixture%ZZ"] {
            assert_eq!(
                parse_handle_input(&format!("https://youtube.com/{reference}")),
                Some(reference.into()),
                "malformed whole handles must reach strict provider validation, not text search"
            );
            assert!(crate::providers::normalize_youtube_channel_handle(reference).is_err());
        }
        let (_temporary, mut controller, requests) = fixture();
        controller.view.search_query = "music @fixture".into();
        controller.dispatch(UiAction::SubmitSearch);
        assert!(requests.try_iter().any(|request| matches!(request,
            ProviderRequest::Search { request, .. } if request.query == "music @fixture")));
    }

    /// Tab, Back, independent input and provider replacement revoke the exact lookup owner.
    #[test]
    fn youtube_handle_lookup_rejects_departed_and_superseded_responses() {
        for departure in ["tab", "back", "search", "provider", "editor", "playing"] {
            let (_temporary, mut controller, requests) = fixture();
            controller.view.search_query = "@fixture".into();
            controller.dispatch(UiAction::SubmitSearch);
            let (generation, provider_generation, _) = take_handle_request(&requests);
            match departure {
                "tab" => {
                    controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
                    controller.dispatch(UiAction::ShowScreen(Screen::Search));
                }
                "back" => controller.dispatch(UiAction::GoBack),
                "editor" => controller.dispatch(UiAction::BeginSearch),
                "playing" => {
                    let item = controller.selected_queue_item().unwrap();
                    controller.current_media = Some(item.media.id.clone());
                    controller.playback_queue.push(item);
                    controller.dispatch(UiAction::ShowNowPlaying);
                }
                "search" => {
                    controller.view.search_query = "another query".into();
                    controller.dispatch(UiAction::SubmitSearch);
                }
                "provider" => controller.youtube_provider_generation += 1,
                _ => unreachable!(),
            }
            let rows = controller.view.rows.clone();
            controller.handle_provider_response(ProviderResponse::ResolvedYouTubeChannel {
                generation,
                provider_generation,
                result: Ok(handle_profile()),
            });
            assert!(!controller.view.youtube_channel_browsing, "{departure}");
            assert!(
                controller.youtube_channel_navigation.is_empty(),
                "{departure}"
            );
            assert_eq!(controller.view.rows, rows, "{departure}");
        }
    }

    /// Lookup errors remain visible instead of silently spending a text-search request.
    #[test]
    fn youtube_handle_lookup_failure_does_not_fall_back_to_search() {
        let (_temporary, mut controller, requests) = fixture();
        controller.view.search_query = "@missing".into();
        controller.dispatch(UiAction::SubmitSearch);
        let (generation, provider_generation, _) = take_handle_request(&requests);
        controller.handle_provider_response(ProviderResponse::ResolvedYouTubeChannel {
            generation,
            provider_generation,
            result: Err("Channel was not found".into()),
        });
        assert!(controller.view.error_popup.is_some());
        assert!(!controller.view.youtube_channel_browsing);
        assert_eq!(controller.view.search_activity, None);
        assert!(
            !requests
                .try_iter()
                .any(|request| matches!(request, ProviderRequest::Search { .. }))
        );
    }

    /// Browsing an unsubscribed uploader owns one source row and never writes OPML.
    #[test]
    fn browse_youtube_channel_opens_one_source_without_subscribing() {
        let (_temporary, mut controller, requests) = fixture();
        let saved = std::fs::read(controller.config.subscriptions_file()).unwrap();
        controller.view.subscriptions.layout = SubscriptionsLayout::Split;
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        assert_eq!(controller.view.screen, Screen::Subscriptions);
        assert_eq!(controller.view.displayed_screen(), Screen::Search);
        assert!(controller.view.youtube_channel_browsing);
        assert_eq!(
            controller.view.subscriptions.layout,
            SubscriptionsLayout::DrillDown
        );
        assert_eq!(
            controller.view.subscriptions.route,
            SubscriptionRoute::Sources
        );
        assert_eq!(
            controller.view.subscriptions.focus,
            SubscriptionPane::Sources
        );
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        assert_eq!(
            controller.view.subscriptions.sources[0].title,
            "Fixture channel"
        );
        assert!(!controller.view.subscriptions.sources[0].subscribed);
        assert_eq!(
            controller.view.details.as_ref().unwrap().channel_id,
            "UCfixture"
        );
        assert!(!controller.view.details.as_ref().unwrap().channel_subscribed);
        assert_eq!(
            std::fs::read(controller.config.subscriptions_file()).unwrap(),
            saved
        );
        assert!(
            !requests
                .try_iter()
                .any(|request| matches!(request, ProviderRequest::ChannelVideos { .. }))
        );
    }

    /// Back returns to the exact search location before considering playback seeks.
    #[test]
    fn browse_youtube_channel_back_restores_exact_search() {
        let (_temporary, mut controller, _requests) = fixture();
        let rows = controller.view.rows.clone();
        let details = controller.view.details.clone();
        let items = controller.youtube_results.clone();
        controller.view.subscriptions.layout = SubscriptionsLayout::Split;
        let playing = MediaId::new(SourceKind::YouTube, "playing");
        controller.current_media = Some(playing.clone());
        controller
            .seek_back
            .push_back((playing.clone(), Duration::from_secs(5)));
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Search);
        assert!(!controller.view.youtube_channel_browsing);
        assert_eq!(controller.view.rows, rows);
        assert_eq!(controller.view.details, details);
        assert_eq!(controller.youtube_results, items);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.youtube_selected, 1);
        assert_eq!(controller.view.search_query, "original search");
        assert_eq!(controller.view.search_cursor_byte, 7);
        assert_eq!(controller.next_youtube_page, Some(3));
        assert_eq!(controller.view.details_scroll, 17);
        assert!(controller.view.details_focused);
        assert_eq!(controller.view.selected_detail_link, Some(2));
        assert_eq!(
            controller.view.subscriptions.layout,
            SubscriptionsLayout::Split
        );
        assert_eq!(controller.current_media, Some(playing));
        assert_eq!(controller.seek_back.len(), 1);
    }

    /// The displayed linked video owns this action even when the list selection differs.
    #[test]
    fn browse_youtube_channel_uses_visible_video_owner() {
        let (_temporary, mut controller, _requests) = fixture();
        let details = controller.view.details.as_mut().unwrap();
        details.media_id = Some(MediaId::new(SourceKind::YouTube, "linked-video"));
        details.channel_id = "UCvisible".into();
        details.channel_name = "Visible uploader".into();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        assert_eq!(
            controller.view.details.as_ref().unwrap().channel_id,
            "UCvisible"
        );
        assert_eq!(
            controller.view.subscriptions.sources[0].title,
            "Visible uploader"
        );
        controller.dispatch(UiAction::GoBack);
        assert_eq!(
            controller.view.details.as_ref().unwrap().media_id,
            Some(MediaId::new(SourceKind::YouTube, "linked-video"))
        );
        assert_eq!(controller.view.selected, 1);
    }

    /// Enter requests the transient source's uploads; successive Back actions restore its parents.
    #[test]
    fn browse_youtube_channel_enter_loads_videos_and_back_walks_parents() {
        let (_temporary, mut controller, requests) = fixture();
        let videos = controller.youtube_results.clone();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = requests
            .try_iter()
            .find_map(|request| match request {
                ProviderRequest::ChannelVideos {
                    generation,
                    request,
                } => Some((generation, request)),
                _ => None,
            })
            .expect("explicit Enter channel videos request");
        assert_eq!(request.channel_id, "UCfixture");
        assert_eq!(
            controller.view.subscriptions.route,
            SubscriptionRoute::Items
        );
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: videos,
                next_page: None,
            }),
        });
        assert_eq!(controller.view.subscriptions.items.len(), 2);
        assert!(controller.selected_queue_item().is_ok());
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Subscriptions);
        assert_eq!(
            controller.view.subscriptions.route,
            SubscriptionRoute::Sources
        );
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        assert!(!controller.view.details.as_ref().unwrap().channel_subscribed);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Search);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.view.details_scroll, 17);
    }

    /// A late child page and old search response cannot overwrite the returned search.
    #[test]
    fn browse_youtube_channel_back_rejects_departed_requests() {
        let (_temporary, mut controller, requests) = fixture();
        let search_generation = controller.search_generation;
        let search_request = controller.youtube_search_request.clone().unwrap();
        let original = controller.view.details.clone();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = requests
            .try_iter()
            .find_map(|request| match request {
                ProviderRequest::ChannelVideos {
                    generation,
                    request,
                } => Some((generation, request)),
                _ => None,
            })
            .unwrap();
        controller.dispatch(UiAction::GoBack);
        controller.dispatch(UiAction::GoBack);
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Err("late channel failure".into()),
        });
        controller.handle_provider_response(ProviderResponse::Search {
            generation: search_generation,
            request: search_request,
            result: Ok(SearchPage {
                page: 1,
                items: Vec::new(),
                next_page: None,
            }),
        });
        assert_eq!(controller.view.rows.len(), 2);
        assert_eq!(controller.view.details, original);
        assert!(controller.view.error_popup.is_none());
        assert_eq!(controller.view.details_scroll, 17);
    }

    /// Explicit subscription changes retain the one-channel page and update its return state.
    #[test]
    fn browse_youtube_channel_subscription_changes_survive_return() {
        for unsubscribe in [false, true] {
            let (_temporary, mut controller, _requests) = fixture();
            controller.dispatch(UiAction::BrowseYouTubeChannel);
            controller.dispatch(UiAction::ToggleSubscription);
            assert_eq!(controller.view.subscriptions.sources.len(), 1);
            assert_eq!(
                controller.view.subscriptions.sources[0].title,
                "Fixture channel"
            );
            assert!(controller.view.details.as_ref().unwrap().channel_subscribed);
            if unsubscribe {
                controller.dispatch(UiAction::ToggleSubscription);
                controller.dispatch(UiAction::ConfirmUnsubscribe {
                    channel_id: "UCfixture".into(),
                });
                assert_eq!(controller.view.subscriptions.sources.len(), 1);
                assert!(!controller.view.details.as_ref().unwrap().channel_subscribed);
            }
            controller.dispatch(UiAction::GoBack);
            assert_eq!(controller.view.screen, Screen::Search);
            assert_eq!(
                controller.view.details.as_ref().unwrap().channel_subscribed,
                !unsubscribe
            );
            assert_eq!(controller.view.rows[1].subscribed, !unsubscribe);
            let saved = subscriptions::load(&controller.config).unwrap();
            assert!(saved.contains_youtube_channel("UCsaved"));
            assert_eq!(saved.contains_youtube_channel("UCfixture"), !unsubscribe);
        }
    }

    /// An explicit Subs tab change restores the saved source tree and configured layout.
    #[test]
    fn browse_youtube_channel_tab_change_restores_normal_subscriptions() {
        let (_temporary, mut controller, _requests) = fixture();
        controller.view.subscriptions.layout = SubscriptionsLayout::Split;
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert!(controller.youtube_channel_navigation.is_empty());
        assert!(!controller.view.youtube_channel_browsing);
        assert_eq!(controller.view.displayed_screen(), Screen::Subscriptions);
        assert_eq!(
            controller.view.subscriptions.layout,
            SubscriptionsLayout::Split
        );
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        assert_eq!(
            controller.view.subscriptions.sources[0].title,
            "Saved channel"
        );
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Subscriptions);
        controller.dispatch(UiAction::ShowScreen(Screen::Search));
        assert_eq!(controller.view.rows.len(), 2);
        assert_eq!(controller.view.selected, 1);
    }

    /// Jumping to the player abandons the transient source before direct route restoration.
    #[test]
    fn browse_youtube_channel_now_playing_abandons_transient_source() {
        let (_temporary, mut controller, _requests) = fixture();
        let item = controller.selected_queue_item().unwrap();
        controller.current_media = Some(item.media.id.clone());
        controller.playback_queue.push(item);
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::ShowNowPlaying);
        assert!(controller.youtube_channel_navigation.is_empty());
        assert_eq!(controller.view.screen, Screen::Search);
        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert_eq!(
            controller.view.subscriptions.sources[0].title,
            "Saved channel"
        );
    }

    /// Replacing the provider invalidates parked pages and all departed channel request owners.
    #[test]
    fn browse_youtube_channel_provider_change_rejects_obsolete_origin() {
        let (_temporary, mut controller, _requests) = fixture();
        controller.view.subscriptions.layout = SubscriptionsLayout::Split;
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::ActivateSelection);
        controller.dispatch(UiAction::GoBack);
        controller.youtube_provider_generation =
            controller.youtube_provider_generation.wrapping_add(1);
        let old_details_generation = controller.details_generation;

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.screen, Screen::Search);
        assert_eq!(controller.view.search_query, "original search");
        assert!(controller.youtube_results.is_empty());
        assert!(controller.youtube_search_request.is_none());
        assert!(controller.next_youtube_page.is_none());
        assert!(controller.active_subscription_channel_id.is_none());
        assert!(!controller.view.subscriptions.loading);
        assert!(controller.youtube_channel_navigation.is_empty());
        assert_eq!(
            controller.view.subscriptions.layout,
            SubscriptionsLayout::Split
        );
        controller.handle_provider_response(ProviderResponse::Details {
            generation: old_details_generation,
            result: Err("departed video metadata failed".into()),
        });
        assert!(controller.view.details.is_none());
        assert!(controller.view.error_popup.is_none());
    }

    /// Channel enrichment can complete while visible, but cannot overwrite the returned video.
    #[test]
    fn browse_youtube_channel_metadata_obeys_current_owner() {
        let (_temporary, mut controller, requests) = fixture();
        let original = controller.view.details.clone();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        let scheduled = controller.scheduled_channel_details.clone().unwrap();
        controller.request_due_channel_details(scheduled.due_at);
        let (generation, provider_generation, channel_id) = requests
            .try_iter()
            .find_map(|request| match request {
                ProviderRequest::ChannelDetails {
                    generation,
                    provider_generation,
                    channel_id,
                } => Some((generation, provider_generation, channel_id)),
                _ => None,
            })
            .unwrap();
        let profile = ChannelDetails {
            summary: ChannelSummary {
                channel_id: channel_id.clone(),
                name: "Enriched channel".into(),
                description: "Public uploader description".into(),
                subscriber_count: Some(42),
                video_count: Some(8),
                created_at: None,
                auto_generated: false,
                thumbnails: Vec::new(),
                webpage_url: None,
            },
            total_view_count: None,
            country: None,
            external_links: Vec::new(),
            external_links_truncated: false,
        };
        controller.handle_provider_response(ProviderResponse::ChannelDetails {
            generation,
            provider_generation,
            channel_id: channel_id.clone(),
            result: Ok(profile.clone()),
        });
        assert_eq!(
            controller.view.details.as_ref().unwrap().description,
            "Public uploader description"
        );
        assert!(!controller.view.details.as_ref().unwrap().channel_subscribed);
        controller.dispatch(UiAction::GoBack);
        controller.handle_provider_response(ProviderResponse::ChannelDetails {
            generation,
            provider_generation,
            channel_id,
            result: Ok(profile),
        });
        assert_eq!(controller.view.details, original);
        assert_eq!(controller.view.details_scroll, 17);
    }

    /// A description link inside channel uploads returns to its video before leaving the channel.
    #[test]
    fn browse_youtube_channel_nested_details_return_before_source() {
        let (_temporary, mut controller, requests) = fixture();
        let videos = controller.youtube_results.clone();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::ActivateSelection);
        let (generation, request) = requests
            .try_iter()
            .find_map(|request| match request {
                ProviderRequest::ChannelVideos {
                    generation,
                    request,
                } => Some((generation, request)),
                _ => None,
            })
            .unwrap();
        controller.handle_provider_response(ProviderResponse::ChannelVideos {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: videos,
                next_page: None,
            }),
        });
        let origin = controller.view.details.clone();
        controller.view.details_scroll = 9;
        controller.dispatch(UiAction::ActivateDescriptionVideo {
            video_id: "9bZkp7q19f0".into(),
            start_seconds: None,
        });
        controller.handle_provider_response(ProviderResponse::Details {
            generation: controller.details_generation,
            result: Ok(linked_video_details(
                "9bZkp7q19f0",
                "Linked child",
                "Description",
            )),
        });
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.details, origin);
        assert_eq!(controller.view.details_scroll, 9);
        assert_eq!(
            controller.view.subscriptions.route,
            SubscriptionRoute::Items
        );
        controller.dispatch(UiAction::GoBack);
        assert_eq!(
            controller.view.subscriptions.route,
            SubscriptionRoute::Sources
        );
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Search);
    }

    /// Hashtag detours retain the transient source and its original search return destination.
    #[test]
    fn browse_youtube_channel_hashtag_detour_preserves_return_chain() {
        let (_temporary, mut controller, requests) = fixture();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::SearchYouTubeHashtag("Music".into()));
        let (generation, request) = requests
            .try_iter()
            .find_map(|request| match request {
                ProviderRequest::Search {
                    generation,
                    request,
                } => Some((generation, request)),
                _ => None,
            })
            .unwrap();
        controller.handle_provider_response(ProviderResponse::Search {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(summary_from_details(
                    &linked_video_details("9bZkp7q19f0", "Hashtag video", "#Music"),
                ))],
                next_page: None,
            }),
        });
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Search);
        assert_eq!(controller.view.search_query, "#Music");
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Subscriptions);
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(controller.view.screen, Screen::Search);
        assert_eq!(controller.view.search_query, "original search");
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.view.details_scroll, 17);
        assert!(controller.youtube_channel_navigation.is_empty());
    }

    /// Fresh queries retire channel and hashtag parents instead of exhausting their bounded stacks.
    #[test]
    fn browse_youtube_channel_new_query_retires_hashtag_channel_parents() {
        for query in ["fresh search", "@fixture"] {
            let (_temporary, mut controller, requests) = fixture();
            for _ in 0..=MAX_CHANNEL_LOCATIONS {
                controller.dispatch(UiAction::BrowseYouTubeChannel);
                assert_eq!(controller.youtube_channel_navigation.len(), 1);
                controller.dispatch(UiAction::SearchYouTubeHashtag("Music".into()));
                let (generation, request) = requests
                    .try_iter()
                    .find_map(|request| match request {
                        ProviderRequest::Search {
                            generation,
                            request,
                        } => Some((generation, request)),
                        _ => None,
                    })
                    .unwrap();
                let page = SearchPage {
                    page: 1,
                    items: vec![SearchItem::Video(summary_from_details(
                        &linked_video_details("9bZkp7q19f0", "Hashtag video", "#Music"),
                    ))],
                    next_page: None,
                };
                controller.handle_provider_response(ProviderResponse::Search {
                    generation,
                    request,
                    result: Ok(page.clone()),
                });
                let rows = controller.view.rows.clone();
                controller.view.search_query = query.into();
                controller.dispatch(UiAction::SubmitSearch);
                assert!(!controller.view.youtube_channel_browsing, "{query}");
                assert!(controller.youtube_channel_navigation.is_empty(), "{query}");
                assert!(controller.youtube_hashtag_history.is_empty(), "{query}");
                if query.starts_with('@') {
                    assert_eq!(
                        controller.view.rows, rows,
                        "a handle parks the current results"
                    );
                    let (generation, provider_generation, _) = take_handle_request(&requests);
                    controller.handle_provider_response(ProviderResponse::ResolvedYouTubeChannel {
                        generation,
                        provider_generation,
                        result: Ok(handle_profile()),
                    });
                    assert_eq!(controller.youtube_channel_navigation.len(), 1);
                    controller.dispatch(UiAction::GoBack);
                    assert_eq!(controller.view.search_query, "#Music");
                    assert_eq!(controller.view.rows, rows);
                    assert!(!controller.view.youtube_channel_browsing);
                } else {
                    let (generation, request) = requests
                        .try_iter()
                        .find_map(|request| match request {
                            ProviderRequest::Search {
                                generation,
                                request,
                            } => Some((generation, request)),
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(request.query, query);
                    controller.handle_provider_response(ProviderResponse::Search {
                        generation,
                        request,
                        result: Ok(page),
                    });
                }
            }
            controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
            assert_eq!(
                controller.view.subscriptions.sources[0].title,
                "Saved channel"
            );
        }
    }

    /// A changed provider cannot leave an older temporary source active after rejecting nested Back.
    #[test]
    fn browse_youtube_channel_provider_change_drops_nested_transient_sources() {
        let (_temporary, mut controller, requests) = fixture();
        controller.view.subscriptions.layout = SubscriptionsLayout::Split;
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::SearchYouTubeHashtag("Music".into()));
        let (generation, request) = requests
            .try_iter()
            .find_map(|request| match request {
                ProviderRequest::Search {
                    generation,
                    request,
                } => Some((generation, request)),
                _ => None,
            })
            .unwrap();
        controller.handle_provider_response(ProviderResponse::Search {
            generation,
            request,
            result: Ok(SearchPage {
                page: 1,
                items: vec![SearchItem::Video(summary_from_details(
                    &linked_video_details("9bZkp7q19f0", "Hashtag video", "#Music"),
                ))],
                next_page: None,
            }),
        });
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        assert_eq!(controller.youtube_channel_navigation.len(), 2);
        controller.youtube_provider_generation =
            controller.youtube_provider_generation.wrapping_add(1);

        controller.dispatch(UiAction::GoBack);

        assert_eq!(controller.view.screen, Screen::Search);
        assert_eq!(controller.view.search_query, "#Music");
        assert!(controller.youtube_channel_navigation.is_empty());
        assert!(controller.youtube_hashtag_history.is_empty());
        assert_eq!(
            controller.view.subscriptions.layout,
            SubscriptionsLayout::Split
        );
        assert!(controller.view.rows.is_empty());
        controller.dispatch(UiAction::ShowScreen(Screen::Subscriptions));
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        assert_eq!(
            controller.view.subscriptions.sources[0].title,
            "Saved channel"
        );
    }

    /// Preferences can change the normal Subs layout without changing temporary channel routing.
    #[test]
    fn browse_youtube_channel_preferences_keep_temporary_drill_down() {
        let (_temporary, mut controller, _requests) = fixture();
        controller.dispatch(UiAction::BrowseYouTubeChannel);
        controller.dispatch(UiAction::OpenPreferences);
        controller
            .view
            .preferences_popup
            .as_mut()
            .unwrap()
            .subscriptions_layout = SubscriptionsLayout::Split;
        controller.dispatch(UiAction::SubmitPreferences);
        assert!(controller.view.preferences_popup.is_none());
        assert_eq!(
            controller.view.subscriptions.layout,
            SubscriptionsLayout::DrillDown
        );
        assert_eq!(controller.view.subscriptions.sources.len(), 1);
        controller.dispatch(UiAction::GoBack);
        assert_eq!(
            controller.view.subscriptions.layout,
            SubscriptionsLayout::Split
        );
    }
}
