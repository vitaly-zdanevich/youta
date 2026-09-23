//! Restores authoritative source rows without replaying media or issuing a search.

use super::*;

#[cfg(feature = "librivox")]
mod librivox;

/// One accepted source context and one reversible focused navigation location.
#[derive(Default)]
pub(super) struct NowPlayingNavigation {
    accepted: Option<(MediaId, Location)>,
    pending: Option<(MediaId, Location)>,
    back: Option<ReturnLocation>,
    local_generation: Option<u64>,
    #[cfg(feature = "librivox")]
    pending_librivox: Option<librivox::PendingReveal>,
}

/// A single moved source page, invalid once a newer search or route owns its rows.
struct ReturnLocation {
    screen: Screen,
    rows: Vec<Option<MediaId>>,
    generation: (u64, u64),
    location: Location,
}

/// Owned source data: accepted contexts retain only their item or owning book.
#[derive(Clone)]
struct Location {
    screen: Screen,
    query: String,
    selected: usize,
    content: Content,
}

/// Real controller adapters used by a focused source page and its Back destination.
#[derive(Clone)]
enum Content {
    Search(Box<SearchLocation>),
    Music {
        items: Vec<SearchItem>,
        start_override: Option<u64>,
    },
    Tracker(Vec<TrackerItem>),
    #[cfg(feature = "bandcamp")]
    Bandcamp {
        items: Vec<BandcampSearchSummary>,
        page: u16,
        next_page: Option<u16>,
    },
    #[cfg(feature = "apple-podcasts")]
    Apple(Box<AppleLocation>),
    #[cfg(feature = "yandex-music")]
    Yandex(Box<YandexMusicNavigationSnapshot>),
    #[cfg(feature = "librivox")]
    LibriVox(Box<LibrivoxNavigationSnapshot>),
    #[cfg(feature = "web-browser")]
    Web(Option<crate::web_browser::WebDirectoryListing>),
}

/// Search adapter state moved intact when revealing a retained direct page or video.
#[derive(Clone, Default)]
struct SearchLocation {
    items: Vec<SearchItem>,
    direct: Option<DirectSourceInput>,
    resolved: Option<ResolvedDirectMedia>,
    local: Vec<LocalMediaItem>,
    next_page: Option<u32>,
    request: Option<SearchRequest>,
    start_override: Option<u64>,
    apple_route: ApplePodcastsRoute,
}

/// The existing podcast adapter state, boxed to keep other source variants compact.
#[cfg(feature = "apple-podcasts")]
#[derive(Clone)]
struct AppleLocation {
    route: ApplePodcastsRoute,
    show: Option<PodcastShowSummary>,
    episodes: Vec<ApplePodcastEpisodeMetadata>,
}

impl AppController {
    /// Preserves the outgoing source and revokes stale UI work without populating a destination.
    pub(super) fn prepare_screen_transition(&mut self, screen: Screen) {
        #[cfg(feature = "archive-org")]
        if screen != Screen::ArchiveOrg {
            self.cancel_archive_now_playing_navigation();
        }
        #[cfg(feature = "librivox")]
        if screen != Screen::LibriVox {
            self.cancel_librivox_now_playing_navigation();
        }
        self.clear_scheduled_subscription_video_metadata();
        if self.view.screen == Screen::History && screen != Screen::History {
            self.cancel_pending_history_replay();
        }
        self.clear_detail_navigation_history();
        #[cfg(feature = "yt-dlp")]
        self.cancel_youtube_prewarm();
        if self.view.screen == Screen::Local && screen != Screen::Local {
            self.cancel_now_playing_local_navigation();
            #[cfg(feature = "acoustid")]
            self.cancel_local_fingerprint();
            #[cfg(feature = "audio-quality")]
            if self.pending_local_audio_quality.is_some() {
                self.cancel_visible_local_audio_quality();
            }
            self.invalidate_local_folder_sizes();
        }
        self.view.search_editing = false;
        #[cfg(feature = "radio")]
        if self.view.screen == Screen::Radio {
            self.radio_filter_edit_snapshot = None;
        }
        match self.view.screen {
            Screen::Search => {
                self.youtube_search_query
                    .clone_from(&self.view.search_query);
                self.youtube_selected = self.view.selected;
            }
            Screen::SoundCloud => {
                self.soundcloud.query.clone_from(&self.view.search_query);
                self.soundcloud.selected = self.view.selected;
                #[cfg(feature = "soundcloud")]
                {
                    self.soundcloud.page_turn = None;
                    self.cancel_soundcloud_catalog_page_turn();
                }
                self.finish_search_activity(SearchActivity::SoundCloud);
            }
            Screen::YouTubeMusic => {
                self.youtube_music_search_query
                    .clone_from(&self.view.search_query);
                self.youtube_music_selected = self.view.selected;
            }
            #[cfg(feature = "yandex-music")]
            Screen::YandexMusic => {
                self.yandex_music_search_query
                    .clone_from(&self.view.search_query);
                self.yandex_music_selected = self.view.selected;
            }
            #[cfg(not(feature = "yandex-music"))]
            Screen::YandexMusic => {}
            #[cfg(feature = "bandcamp")]
            Screen::Bandcamp => {
                self.bandcamp_search_query
                    .clone_from(&self.view.search_query);
                self.bandcamp_selected = self.view.selected;
                self.cancel_pending_bandcamp_resolution();
            }
            Screen::ApplePodcasts => {
                self.apple_podcasts_search_query
                    .clone_from(&self.view.search_query);
                match self.apple_podcasts_route {
                    ApplePodcastsRoute::Shows => {
                        self.apple_podcasts_selected = self.view.selected;
                    }
                    ApplePodcastsRoute::Episodes => {
                        self.apple_podcast_episode_selected = self.view.selected;
                    }
                    ApplePodcastsRoute::Direct => {}
                }
            }
            #[cfg(feature = "web-browser")]
            Screen::Web => {
                self.web.selected = self.view.selected;
                self.finish_search_activity(SearchActivity::Web);
            }
            Screen::ArchiveOrg => {
                if self.archive_org_search_scope == crate::domain::ArchiveOrgSearchScope::Text {
                    self.archive_org_search_query
                        .clone_from(&self.view.search_query);
                }
                self.archive_org_selected = self.view.selected;
                self.finish_search_activity(SearchActivity::ArchiveOrg);
            }
            Screen::LibriVox => {
                self.librivox_search_query
                    .clone_from(&self.view.search_query);
                self.librivox_selected = self.view.selected;
            }
            Screen::Radio => {
                self.radio_filter_query.clone_from(&self.view.search_query);
                #[cfg(feature = "radio")]
                self.remember_selected_radio_station();
                #[cfg(not(feature = "radio"))]
                {
                    self.radio_selected = self.view.selected;
                }
            }
            Screen::TrackerMusic => {
                self.tracker_search_query
                    .clone_from(&self.view.search_query);
                self.tracker_selected = self.view.selected;
            }
            Screen::Playlists => match self.playlists_route {
                PlaylistsRoute::Index => self.playlist_selected = self.view.selected,
                PlaylistsRoute::Entries { .. } => {
                    self.playlist_entry_selected = self.view.selected;
                }
            },
            _ => {}
        }
        self.details_generation = self.details_generation.wrapping_add(1);
        #[cfg(feature = "wikidata")]
        self.invalidate_wikidata_lookup();
        self.channel_details_generation = self.channel_details_generation.wrapping_add(1);
        self.scheduled_channel_details = None;
    }

    /// Captures source metadata only after the backend accepts this exact identity.
    pub(super) fn remember_now_playing_context(&mut self, item: &QueueItem) {
        #[cfg(feature = "archive-org")]
        self.cancel_archive_now_playing_navigation();
        #[cfg(feature = "librivox")]
        self.cancel_changed_librivox_now_playing_navigation(&item.media.id);
        let id = &item.media.id;
        let pending = self
            .now_playing_navigation
            .pending
            .as_ref()
            .filter(|(owner, _)| owner == id)
            .map(|(_, location)| location.clone());
        let location = pending.or_else(|| self.now_playing_location(item));
        if let Some(location) = location {
            self.now_playing_navigation.accepted = Some((id.clone(), location));
        } else if self
            .now_playing_navigation
            .accepted
            .as_ref()
            .is_some_and(|(owner, _)| owner != id)
        {
            self.now_playing_navigation.accepted = None;
        }
    }

    /// Reopens retained typed metadata after the original provider list was replaced.
    pub(super) fn reveal_retained_now_playing(&mut self, item: &QueueItem) -> bool {
        let Some((_, location)) =
            self.now_playing_navigation
                .accepted
                .as_ref()
                .filter(|(owner, _)| {
                    owner == &item.media.id && self.current_media.as_ref() == Some(owner)
                })
        else {
            return false;
        };
        let screen = location.screen;
        let location = location.clone();
        let previous = self.replace_now_playing_location(location);
        self.now_playing_navigation.back = Some(ReturnLocation {
            screen,
            rows: self
                .view
                .rows
                .iter()
                .map(|row| row.media_id.clone())
                .collect(),
            generation: self.now_playing_location_generation(screen),
            location: previous,
        });
        true
    }

    /// Restores the source location displaced by an explicit now-playing reveal.
    pub(super) fn restore_now_playing_location(&mut self) -> bool {
        let Some(back) = self.now_playing_navigation.back.as_ref() else {
            return false;
        };
        if back.screen != self.view.screen {
            return false;
        }
        if back.generation != self.now_playing_location_generation(back.screen)
            || !back
                .rows
                .iter()
                .eq(self.view.rows.iter().map(|row| &row.media_id))
        {
            self.now_playing_navigation.back = None;
            return false;
        }
        let back = self
            .now_playing_navigation
            .back
            .take()
            .expect("checked above");
        self.replace_now_playing_location(back.location);
        self.refresh_selected_playlist_state();
        self.view.status_line = "Returned to the previous source page".into();
        true
    }

    /// Retires accepted metadata when playback is explicitly cleared.
    pub(super) fn clear_now_playing_context(&mut self) {
        #[cfg(feature = "archive-org")]
        self.cancel_archive_now_playing_navigation();
        #[cfg(feature = "librivox")]
        self.cancel_librivox_now_playing_navigation();
        self.now_playing_navigation.accepted = None;
        self.view.playing_screen = None;
    }

    /// Labels the accepted provider without guessing from the tab currently browsed.
    pub(super) fn update_now_playing_screen(
        &mut self,
        item: &QueueItem,
        origin: Option<&AutoplayOrigin>,
    ) {
        let retained = self
            .now_playing_navigation
            .accepted
            .as_ref()
            .filter(|(owner, _)| owner == &item.media.id)
            .map(|(_, location)| location.screen);
        let screen = match item.media.id.source {
            SourceKind::YouTube
                if matches!(origin, Some(AutoplayOrigin::YouTubeMusic { .. }))
                    || item.media.webpage_url.host_str() == Some("music.youtube.com") =>
            {
                Screen::YouTubeMusic
            }
            SourceKind::YouTube if matches!(origin, Some(AutoplayOrigin::YouTube { .. })) => {
                Screen::Search
            }
            _ if retained.is_some() => retained.expect("checked above"),
            SourceKind::YouTube => Screen::Search,
            SourceKind::SoundCloud => Screen::SoundCloud,
            SourceKind::YandexMusic => Screen::YandexMusic,
            SourceKind::Bandcamp => Screen::Bandcamp,
            SourceKind::ApplePodcasts => Screen::ApplePodcasts,
            SourceKind::ArchiveOrg => Screen::ArchiveOrg,
            SourceKind::LibriVox => Screen::LibriVox,
            SourceKind::Rss => Screen::Subscriptions,
            SourceKind::Radio | SourceKind::BbcRadio => Screen::Radio,
            SourceKind::ModArchive => Screen::TrackerMusic,
            SourceKind::Local => Screen::Local,
            SourceKind::RemoteFiles => Screen::Web,
            _ => Screen::Search,
        };
        self.view.playing_screen = Some(screen);
        if item.media.id.source == SourceKind::YouTube
            && let Some((_, context)) = self
                .now_playing_navigation
                .accepted
                .as_mut()
                .filter(|(owner, _)| owner == &item.media.id)
            && context.screen != screen
        {
            let items = match &mut context.content {
                Content::Search(search) => std::mem::take(&mut search.items),
                Content::Music { items, .. } => std::mem::take(items),
                _ => return,
            };
            context.screen = screen;
            context.content = if screen == Screen::YouTubeMusic {
                Content::Music {
                    items,
                    start_override: None,
                }
            } else {
                Content::Search(Box::new(SearchLocation {
                    items,
                    ..SearchLocation::default()
                }))
            };
        }
    }

    /// Temporarily exposes resolver metadata to the successful backend-acceptance hook.
    pub(super) fn play_resolved_with_navigation_context(
        &mut self,
        item: QueueItem,
        media: &ResolvedDirectMedia,
    ) {
        let screen = if media.source == SourceKind::ApplePodcasts {
            Screen::ApplePodcasts
        } else {
            Screen::Search
        };
        let location = Location {
            screen,
            query: media
                .webpage_url
                .as_ref()
                .map_or_else(String::new, ToString::to_string),
            selected: 0,
            content: Content::Search(Box::new(SearchLocation {
                resolved: Some(media.clone()),
                apple_route: ApplePodcastsRoute::Direct,
                ..SearchLocation::default()
            })),
        };
        self.now_playing_navigation.pending = Some((item.media.id.clone(), location));
        self.play_queue_item(item, false);
        self.now_playing_navigation.pending = None;
    }

    /// Preserves the canonical release for a resolved History/playlist playback as well.
    #[cfg(feature = "bandcamp")]
    pub(super) fn play_bandcamp_with_navigation_context(
        &mut self,
        item: QueueItem,
        summary: BandcampSearchSummary,
        origin: Option<AutoplayOrigin>,
        input: PlaybackInput,
    ) {
        let location = Location {
            screen: Screen::Bandcamp,
            query: summary.webpage_url.to_string(),
            selected: 0,
            content: Content::Bandcamp {
                items: vec![summary],
                page: 1,
                next_page: None,
            },
        };
        self.now_playing_navigation.pending = Some((item.media.id.clone(), location));
        self.play_queue_item_with_origin_and_input(item, false, origin, Some(input));
        self.now_playing_navigation.pending = None;
    }

    /// Copies only the exact typed item (or its owning book), not a provider's result set.
    fn now_playing_location(&self, item: &QueueItem) -> Option<Location> {
        let id = &item.media.id;
        let (screen, content, selected) = match id.source {
            SourceKind::YouTube => {
                let music = item.media.webpage_url.host_str() == Some("music.youtube.com")
                    || self.view.screen == Screen::YouTubeMusic;
                let find = |items: &[SearchItem]| {
                    items.iter().find(|candidate| matches!(candidate, SearchItem::Video(video) if video.video_id == id.external_id)).cloned()
                };
                if music && let Some(video) = find(&self.youtube_music_results) {
                    (
                        Screen::YouTubeMusic,
                        Content::Music {
                            items: vec![video],
                            start_override: None,
                        },
                        0,
                    )
                } else if let Some(video) = find(&self.youtube_results) {
                    (
                        Screen::Search,
                        Content::Search(Box::new(SearchLocation {
                            items: vec![video],
                            ..SearchLocation::default()
                        })),
                        0,
                    )
                } else if let Some(video) = find(&self.youtube_music_results) {
                    (
                        Screen::YouTubeMusic,
                        Content::Music {
                            items: vec![video],
                            start_override: None,
                        },
                        0,
                    )
                } else {
                    validate_youtube_video_id(&id.external_id).ok()?;
                    // This is the existing canonical direct-video adapter, populated
                    // only with metadata already accepted into the playing queue.
                    // Missing channel IDs stay unknown; selection requests full details
                    // through the ordinary generation-checked provider lane.
                    let video = SearchItem::Video(VideoSummary {
                        video_id: id.external_id.clone(),
                        title: item.media.title.clone(),
                        channel_name: item.media.creator.clone().unwrap_or_default(),
                        channel_id: String::new(),
                        description: item.media.description.clone().unwrap_or_default(),
                        duration_seconds: item.media.duration_seconds,
                        view_count: item.media.statistics.views,
                        published_at: item.media.published_at,
                        published_text: None,
                        live: item.media.kind == MediaKind::LiveStream,
                        orientation: VideoOrientation::Unknown,
                        thumbnails: Vec::new(),
                        webpage_url: url::Url::parse(&youtube_video_url(&id.external_id)).ok(),
                        stream_url: None,
                    });
                    if music {
                        (
                            Screen::YouTubeMusic,
                            Content::Music {
                                items: vec![video],
                                start_override: None,
                            },
                            0,
                        )
                    } else {
                        (
                            Screen::Search,
                            Content::Search(Box::new(SearchLocation {
                                items: vec![video],
                                ..SearchLocation::default()
                            })),
                            0,
                        )
                    }
                }
            }
            #[cfg(feature = "bandcamp")]
            SourceKind::Bandcamp => (
                Screen::Bandcamp,
                Content::Bandcamp {
                    items: vec![
                        self.bandcamp_results
                            .iter()
                            .find(|summary| &summary.id == id)?
                            .clone(),
                    ],
                    page: 1,
                    next_page: None,
                },
                0,
            ),
            #[cfg(feature = "apple-podcasts")]
            SourceKind::ApplePodcasts
                if self
                    .apple_podcast_episodes
                    .iter()
                    .any(|episode| episode.episode_id.to_string() == id.external_id) =>
            {
                (
                    Screen::ApplePodcasts,
                    Content::Apple(Box::new(AppleLocation {
                        route: ApplePodcastsRoute::Episodes,
                        show: self.active_apple_podcast_show.clone(),
                        episodes: vec![
                            self.apple_podcast_episodes
                                .iter()
                                .find(|episode| episode.episode_id.to_string() == id.external_id)?
                                .clone(),
                        ],
                    })),
                    0,
                )
            }
            #[cfg(feature = "yandex-music")]
            SourceKind::YandexMusic => {
                let row = self.yandex_music_rows.iter()
                    .find(|row| matches!(row, YandexMusicRow::Track(track) if track.id == id.external_id))
                    .cloned().or_else(|| accepted_yandex_track(item).map(|track| YandexMusicRow::Track(Box::new(track))))?;
                (
                    Screen::YandexMusic,
                    Content::Yandex(Box::new(YandexMusicNavigationSnapshot {
                        route: YandexMusicRoute::Search,
                        rows: vec![row],
                        selected: 0,
                        album: None,
                        artist_id: None,
                    })),
                    0,
                )
            }
            #[cfg(feature = "librivox")]
            SourceKind::LibriVox => {
                let book = self.active_librivox_book.as_ref()?;
                let index = book.sections.iter().position(|section| {
                    librivox_section_media_id(book.book_id, section.section_id) == *id
                })?;
                (
                    Screen::LibriVox,
                    Content::LibriVox(Box::new(LibrivoxNavigationSnapshot {
                        route: LibrivoxRoute::Book,
                        books: Vec::new(),
                        active_book: Some(book.clone()),
                        active_author: None,
                        selected: index,
                        query: self.librivox_search_query.clone(),
                    })),
                    index,
                )
            }
            SourceKind::ModArchive => (
                Screen::TrackerMusic,
                Content::Tracker(vec![
                    self.tracker_results
                        .iter()
                        .find(|track| tracker_item_matches_media_id(track, id))?
                        .clone(),
                ]),
                0,
            ),
            #[cfg(feature = "web-browser")]
            SourceKind::RemoteFiles
                if self.web.listing.as_ref().is_some_and(|listing| {
                    listing.entries.iter().any(|entry| {
                        web::queue_item_from_web(entry)
                            .is_some_and(|candidate| candidate.media.id == *id)
                    })
                }) =>
            {
                let listing = self.web.listing.as_ref()?;
                let entry = listing
                    .entries
                    .iter()
                    .find(|entry| {
                        web::queue_item_from_web(entry)
                            .is_some_and(|candidate| candidate.media.id == *id)
                    })?
                    .clone();
                let mut listing = listing.clone();
                listing.entries = vec![entry];
                let index = usize::from(listing.parent.is_some());
                (Screen::Web, Content::Web(Some(listing)), index)
            }
            SourceKind::ArchiveOrg
            | SourceKind::Local
            | SourceKind::Radio
            | SourceKind::SoundCloud => return None,
            #[cfg(not(feature = "librivox"))]
            SourceKind::LibriVox => return None,
            #[cfg(not(feature = "yandex-music"))]
            SourceKind::YandexMusic => return None,
            _ => {
                let media = self
                    .resolved_direct
                    .as_ref()
                    .filter(|media| {
                        media.source == id.source && media.external_id == id.external_id
                    })
                    .cloned()
                    .or_else(|| accepted_direct_media(item))?;
                let screen = if id.source == SourceKind::ApplePodcasts {
                    Screen::ApplePodcasts
                } else {
                    Screen::Search
                };
                (
                    screen,
                    Content::Search(Box::new(SearchLocation {
                        resolved: Some(media),
                        apple_route: ApplePodcastsRoute::Direct,
                        ..SearchLocation::default()
                    })),
                    0,
                )
            }
        };
        Some(Location {
            screen,
            query: self.source_location_query(screen).to_owned(),
            selected,
            content,
        })
    }

    /// Returns an independent tab query, including edits made on its current page.
    fn source_location_query(&self, screen: Screen) -> &str {
        if self.view.screen == screen {
            return &self.view.search_query;
        }
        match screen {
            Screen::YouTubeMusic => &self.youtube_music_search_query,
            Screen::YandexMusic => &self.yandex_music_search_query,
            #[cfg(feature = "bandcamp")]
            Screen::Bandcamp => &self.bandcamp_search_query,
            Screen::ApplePodcasts => &self.apple_podcasts_search_query,
            Screen::LibriVox => &self.librivox_search_query,
            Screen::TrackerMusic => &self.tracker_search_query,
            #[cfg(feature = "web-browser")]
            Screen::Web => &self.web.query,
            _ => &self.youtube_search_query,
        }
    }

    /// Combines global and provider-local ownership for a reversible focused page.
    fn now_playing_location_generation(&self, screen: Screen) -> (u64, u64) {
        let provider = match screen {
            #[cfg(feature = "yandex-music")]
            Screen::YandexMusic => self.yandex_music_generation,
            #[cfg(feature = "librivox")]
            Screen::LibriVox => self.librivox_generation,
            #[cfg(feature = "web-browser")]
            Screen::Web => self.web.generation,
            _ => 0,
        };
        (self.search_generation, provider)
    }

    /// Moves the displaced source page into one Back slot, then projects real typed data.
    fn replace_now_playing_location(&mut self, mut location: Location) -> Location {
        use std::mem::swap;
        let screen = location.screen;
        let query = location.query.clone();
        let selected = location.selected;
        location.query = self.source_location_query(screen).to_owned();
        location.selected = if self.view.screen == screen {
            self.view.selected
        } else {
            match screen {
                Screen::YouTubeMusic => self.youtube_music_selected,
                Screen::YandexMusic => self.yandex_music_selected,
                #[cfg(feature = "bandcamp")]
                Screen::Bandcamp => self.bandcamp_selected,
                Screen::ApplePodcasts
                    if self.apple_podcasts_route == ApplePodcastsRoute::Episodes =>
                {
                    self.apple_podcast_episode_selected
                }
                Screen::ApplePodcasts => self.apple_podcasts_selected,
                Screen::LibriVox => self.librivox_selected,
                Screen::TrackerMusic => self.tracker_selected,
                #[cfg(feature = "web-browser")]
                Screen::Web => self.web.selected,
                _ => self.youtube_selected,
            }
        };
        self.supersede_search_generation();
        match &mut location.content {
            Content::Search(search) => {
                swap(&mut search.items, &mut self.youtube_results);
                swap(&mut search.direct, &mut self.direct_item);
                swap(&mut search.resolved, &mut self.resolved_direct);
                swap(&mut search.local, &mut self.local_results);
                swap(&mut search.next_page, &mut self.next_youtube_page);
                swap(&mut search.request, &mut self.youtube_search_request);
                swap(
                    &mut search.start_override,
                    &mut self.selected_start_override,
                );
                if screen == Screen::ApplePodcasts {
                    swap(&mut search.apple_route, &mut self.apple_podcasts_route);
                }
            }
            Content::Music {
                items,
                start_override,
            } => {
                swap(items, &mut self.youtube_music_results);
                swap(start_override, &mut self.youtube_music_start_override);
            }
            Content::Tracker(items) => swap(items, &mut self.tracker_results),
            #[cfg(feature = "bandcamp")]
            Content::Bandcamp {
                items,
                page,
                next_page,
            } => {
                swap(items, &mut self.bandcamp_results);
                swap(page, &mut self.bandcamp_page);
                swap(next_page, &mut self.bandcamp_next_page);
            }
            #[cfg(feature = "apple-podcasts")]
            Content::Apple(snapshot) => {
                self.pending_apple_podcast_episodes = None;
                swap(&mut snapshot.route, &mut self.apple_podcasts_route);
                swap(&mut snapshot.show, &mut self.active_apple_podcast_show);
                swap(&mut snapshot.episodes, &mut self.apple_podcast_episodes);
            }
            #[cfg(feature = "yandex-music")]
            Content::Yandex(snapshot) => {
                self.yandex_music_generation = self.yandex_music_generation.wrapping_add(1);
                swap(&mut snapshot.route, &mut self.yandex_music_route);
                swap(&mut snapshot.rows, &mut self.yandex_music_rows);
                swap(&mut snapshot.album, &mut self.yandex_music_album);
                swap(&mut snapshot.artist_id, &mut self.yandex_music_artist_id);
            }
            #[cfg(feature = "librivox")]
            Content::LibriVox(snapshot) => {
                self.librivox_generation = self.librivox_generation.wrapping_add(1);
                self.pending_librivox_request = None;
                swap(&mut snapshot.route, &mut self.librivox_route);
                swap(&mut snapshot.books, &mut self.librivox_books);
                swap(&mut snapshot.active_book, &mut self.active_librivox_book);
                swap(
                    &mut snapshot.active_author,
                    &mut self.active_librivox_author,
                );
            }
            #[cfg(feature = "web-browser")]
            Content::Web(listing) => {
                self.cancel_web_navigation_for_now_playing();
                swap(listing, &mut self.web.listing);
            }
        }
        if self.view.screen != screen {
            self.show_screen(screen);
        } else {
            self.prepare_screen_transition(screen);
        }
        self.clear_detail_navigation_history();
        self.details_generation = self.details_generation.wrapping_add(1);
        self.channel_details_generation = self.channel_details_generation.wrapping_add(1);
        self.scheduled_channel_details = None;
        self.view.search_query = query.clone();
        self.view.selected = selected;
        match screen {
            Screen::YouTubeMusic => {
                self.youtube_music_search_query = query;
                self.youtube_music_selected = selected;
            }
            Screen::YandexMusic => {
                self.yandex_music_search_query = query;
                self.yandex_music_selected = selected;
            }
            #[cfg(feature = "bandcamp")]
            Screen::Bandcamp => {
                self.bandcamp_search_query = query;
                self.bandcamp_selected = selected;
            }
            Screen::ApplePodcasts => {
                self.apple_podcasts_search_query = query;
                self.apple_podcast_episode_selected = selected;
            }
            Screen::LibriVox => {
                self.librivox_search_query = query;
                self.librivox_selected = selected;
            }
            Screen::TrackerMusic => {
                self.tracker_search_query = query;
                self.tracker_selected = selected;
            }
            #[cfg(feature = "web-browser")]
            Screen::Web => {
                self.web.query = query;
                self.web.selected = selected;
            }
            _ => {
                self.youtube_search_query = query;
                self.youtube_selected = selected;
            }
        }
        self.populate_local_screen();
        match screen {
            Screen::Search if self.resolved_direct.is_some() => apply_resolved_direct_view(
                &self.store,
                &mut self.view,
                self.resolved_direct.as_ref().expect("checked above"),
            ),
            Screen::Search | Screen::YouTubeMusic => self.request_selected_details(),
            #[cfg(feature = "yandex-music")]
            Screen::YandexMusic => self.update_yandex_music_detail(),
            #[cfg(feature = "apple-podcasts")]
            Screen::ApplePodcasts if self.apple_podcasts_route == ApplePodcastsRoute::Episodes => {
                self.update_apple_podcast_episode_detail()
            }
            #[cfg(feature = "web-browser")]
            Screen::Web => self.update_web_detail(),
            _ => {}
        }
        location
    }
    /// Reveals existing typed provider data before using the legacy queue-only fallback.
    pub(super) fn reveal_cached_now_playing(&mut self, item: &QueueItem) -> bool {
        let id = &item.media.id;
        #[cfg(feature = "rss")]
        if id.source == SourceKind::Rss && self.reveal_playing_rss(id) {
            return true;
        }
        if id.source == SourceKind::Local
            && let Some(listing) = self.local_listing.as_ref()
            && let Some(index) = listing
                .entries
                .iter()
                .position(|entry| local_path_matches_media_id(&entry.path, id))
        {
            let selected = index.saturating_add(usize::from(listing.parent.is_some()));
            self.show_screen(Screen::Local);
            self.view.selected = selected;
            self.refresh_local_browser_rows();
            self.update_local_browser_detail();
            return true;
        }
        #[cfg(feature = "archive-org")]
        if id.source == SourceKind::ArchiveOrg && self.reveal_playing_archive_org(id) {
            return true;
        }
        #[cfg(feature = "soundcloud")]
        if id.source == SourceKind::SoundCloud && self.reveal_playing_soundcloud(id) {
            return true;
        }
        #[cfg(feature = "yandex-music")]
        if id.source == SourceKind::YandexMusic
            && let Some(index) = self.yandex_music_rows.iter().position(
                |row| matches!(row, YandexMusicRow::Track(track) if track.id == id.external_id),
            )
        {
            self.show_screen(Screen::YandexMusic);
            self.yandex_music_selected = index;
            self.view.selected = index;
            self.update_yandex_music_detail();
            return true;
        }
        #[cfg(feature = "bandcamp")]
        if id.source == SourceKind::Bandcamp
            && let Some(index) = self
                .bandcamp_results
                .iter()
                .position(|release| release.id == *id)
        {
            self.show_screen(Screen::Bandcamp);
            self.bandcamp_selected = index;
            self.view.selected = index;
            self.update_bandcamp_detail();
            return true;
        }
        #[cfg(feature = "apple-podcasts")]
        if id.source == SourceKind::ApplePodcasts
            && let Some(index) = self
                .apple_podcast_episodes
                .iter()
                .position(|episode| episode.episode_id.to_string() == id.external_id)
        {
            self.apple_podcasts_route = ApplePodcastsRoute::Episodes;
            self.show_screen(Screen::ApplePodcasts);
            self.apple_podcast_episode_selected = index;
            self.view.selected = index;
            self.update_apple_podcast_episode_detail();
            return true;
        }
        #[cfg(feature = "librivox")]
        if id.source == SourceKind::LibriVox
            && let Some(book) = self.active_librivox_book.as_ref()
            && let Some(index) = book.sections.iter().position(|section| {
                librivox_section_media_id(book.book_id, section.section_id) == *id
            })
        {
            self.librivox_route = LibrivoxRoute::Book;
            self.show_screen(Screen::LibriVox);
            self.librivox_selected = index;
            self.view.selected = index;
            self.update_librivox_detail();
            return true;
        }
        #[cfg(feature = "web-browser")]
        if id.source == SourceKind::RemoteFiles
            && !self.web.pending
            && let Some(listing) = self.web.listing.as_ref()
            && let Some(index) = listing.entries.iter().position(|entry| {
                web::queue_item_from_web(entry).is_some_and(|candidate| candidate.media.id == *id)
            })
        {
            let selected = index.saturating_add(usize::from(listing.parent.is_some()));
            self.show_screen(Screen::Web);
            self.view.selected = selected;
            self.update_web_detail();
            return true;
        }

        // Direct-only providers already have a real controller adapter backing
        // their single Search row. Do not replace another active search's data.
        if self.local_results.is_empty()
            && let Some(media) = self
                .resolved_direct
                .as_ref()
                .filter(|media| media.source == id.source && media.external_id == id.external_id)
                .cloned()
        {
            #[cfg(feature = "apple-podcasts")]
            let screen = if id.source == SourceKind::ApplePodcasts {
                self.apple_podcasts_route = ApplePodcastsRoute::Direct;
                Screen::ApplePodcasts
            } else {
                Screen::Search
            };
            #[cfg(not(feature = "apple-podcasts"))]
            let screen = Screen::Search;
            self.show_screen(screen);
            apply_resolved_direct_view(&self.store, &mut self.view, &media);
            self.view.selected = 0;
            return true;
        }
        false
    }

    /// Selects an RSS episode only through a still-subscribed, authoritative cached feed.
    #[cfg(feature = "rss")]
    fn reveal_playing_rss(&mut self, id: &MediaId) -> bool {
        let entries = self.subscription_tree.flattened_subscriptions();
        let Some((source_index, feed_url, index)) =
            entries
                .iter()
                .enumerate()
                .find_map(|(source_index, entry)| {
                    if entry.subscription.kind != SubscriptionKind::Rss {
                        return None;
                    }
                    let url = entry.subscription.url.as_str();
                    self.subscription_video_cache
                        .get(url)?
                        .items
                        .iter()
                        .position(|candidate| {
                            subscription_item_media_id(candidate).as_ref() == Some(id)
                        })
                        .map(|index| (source_index, url.to_owned(), index))
                })
        else {
            return false;
        };
        self.prepare_screen_transition(Screen::Subscriptions);
        self.subscription_generation = self.subscription_generation.wrapping_add(1);
        self.pending_rss_subscription_refresh = None;
        self.pending_subscription_refresh = None;
        self.clear_subscription_loading_state();
        self.clear_detail_navigation_history();
        self.details_generation = self.details_generation.wrapping_add(1);
        self.view.screen = Screen::Subscriptions;
        self.subscription_entries = entries;
        self.rebuild_subscription_source_rows();
        self.view.subscriptions.selected_source = source_index;
        self.update_selected_subscription_source();
        self.active_subscription_channel_id = None;
        self.active_subscription_rss_url = Some(feed_url);
        self.view.subscriptions.route = SubscriptionRoute::Items;
        self.view.subscriptions.focus = SubscriptionPane::Items;
        self.view.subscriptions.description_expanded =
            self.view.subscriptions.layout == SubscriptionsLayout::Split;
        self.refresh_subscription_video_rows();
        self.view.subscriptions.selected_item = index;
        self.update_selected_rss_episode_detail();
        true
    }

    /// Loads the exact playing file's parent using the existing bounded directory worker.
    pub(super) fn reveal_uncached_local_now_playing(&mut self, item: &QueueItem) -> bool {
        if item.media.id.source != SourceKind::Local {
            return false;
        }
        let Ok(path) = item.media.webpage_url.to_file_path() else {
            return false;
        };
        if !path.is_file() || !local_path_matches_media_id(&path, &item.media.id) {
            return false;
        }
        let Some(directory) = path.parent().map(Path::to_path_buf) else {
            return false;
        };
        self.show_screen(Screen::Local);
        self.browse_local_directory_with_reselection(directory, Some(path));
        self.now_playing_navigation.local_generation = Some(self.local_generation);
        self.view.right_panel_mode = RightPanelMode::Details;
        self.view.details_focused = true;
        true
    }

    /// Abandons only a directory request started by this navigation action.
    pub(super) fn cancel_now_playing_local_navigation(&mut self) {
        if self.now_playing_navigation.local_generation.take() == Some(self.local_generation) {
            self.local_generation = self.local_generation.wrapping_add(1);
            self.pending_local_reselection = None;
            self.view.local_browse_pending = false;
        }
    }

    /// Finishes a proven source selection without changing the playing queue or backend.
    pub(super) fn finish_now_playing_selection(&mut self, item: &QueueItem) {
        if self.current_media.as_ref() == Some(&item.media.id) {
            self.view.playing_screen = Some(self.view.screen);
        }
        self.view.right_panel_mode = RightPanelMode::Details;
        self.view.details_focused = true;
        self.view.details_scroll = 0;
        self.view.selected_detail_link = None;
        self.view.detail_link_reveal = None;
        self.view.status_line = format!("Selected playing item: {}", item.media.title);
        self.refresh_selected_playlist_state();
    }
}

/// Adapts accepted queue facts into the same real direct source row used by URL resolution.
/// Unknown facts stay absent; canonical identity and playback URLs are never invented.
fn accepted_direct_media(item: &QueueItem) -> Option<ResolvedDirectMedia> {
    let webpage_url = stable_history_remote_url(&item.media.id.source, &item.media.webpage_url)?;
    let playback_url = url::Url::parse(&item.playback_location).ok()?;
    if !matches!(playback_url.scheme(), "https" | "http")
        || !playback_url.username().is_empty()
        || playback_url.password().is_some()
    {
        return None;
    }
    Some(ResolvedDirectMedia {
        source: item.media.id.source.clone(),
        external_id: item.media.id.external_id.clone(),
        title: item.media.title.clone(),
        row_subtitle: item.media.creator.clone().unwrap_or_default(),
        description: item.media.description.clone().unwrap_or_default(),
        license: match &item.media.license {
            MediaLicense::CreativeCommons(value) | MediaLicense::Other(value) => value.clone(),
            MediaLicense::PublicDomain => "Public domain".into(),
            MediaLicense::YouTubeStandard => "YouTube Standard".into(),
            MediaLicense::Unknown => String::new(),
        },
        published: item.media.published_at.map(format_unix_utc_date),
        artwork_url: item.media.thumbnail_url.clone(),
        duration_seconds: item.media.duration_seconds,
        playback_url: Some(playback_url),
        webpage_url: Some(webpage_url),
        status_line: String::new(),
    })
}

/// Reuses playback's queue adapter only when its public track URL proves the same identity.
#[cfg(feature = "yandex-music")]
fn accepted_yandex_track(item: &QueueItem) -> Option<YandexMusicTrack> {
    let url = &item.media.webpage_url;
    let id = &item.media.id.external_id;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("music.yandex.ru" | "music.yandex.com"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || id.is_empty()
        || id.len() > 100
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'))
    {
        return None;
    }
    let segments = url.path_segments()?.collect::<Vec<_>>();
    let matches = matches!(segments.as_slice(), ["track", track] if *track == id)
        || matches!(segments.as_slice(), ["album", album, "track", track] if !album.is_empty() && *track == id);
    matches.then(|| yandex_music_track_from_queue_item(item))
}
