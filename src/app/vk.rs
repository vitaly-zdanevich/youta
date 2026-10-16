//! Explicit-session VK music navigation and bounded, notified native HTTP work.
//!
//! Only canonical public audio pages cross the queue, view and persistence
//! boundaries. Cookie imports, reload keys and signed media URLs remain owned
//! by one source-specific worker lane and are never delegated to yt-dlp.

use super::*;
#[cfg(feature = "vk")]
use crate::domain::VkSearchKind;

#[cfg(feature = "vk")]
use crate::providers::vk::{
    VkAlbum, VkAlbumPage, VkAudioId, VkClient, VkPage, VkPageCursor, VkPlaybackUrl, VkQuery,
    VkTrack,
};

/// Keeps navigation independent even when the source is omitted from a build.
#[derive(Default)]
pub(super) struct VkState {
    pub(super) query: String,
    pub(super) selected: usize,
    /// Displayed content can be tracks while the search selector remains Albums.
    #[cfg(feature = "vk")]
    results_kind: VkSearchKind,
    #[cfg(feature = "vk")]
    albums: Vec<VkAlbum>,
    #[cfg(feature = "vk")]
    items: Vec<VkTrack>,
    #[cfg(feature = "vk")]
    next: Option<VkPageCursor>,
    #[cfg(feature = "vk")]
    generation: u64,
    /// Retains a sanitized failure only for its current catalogue generation and imported session.
    #[cfg(feature = "vk")]
    search_error: Option<(u64, String)>,
    #[cfg(feature = "vk")]
    playback_generation: u64,
    #[cfg(feature = "vk")]
    pending_search: Option<SearchJob>,
    #[cfg(feature = "vk")]
    pending_browse: Option<BrowseJob>,
    /// Public destination/title; private request references stay in provider records.
    #[cfg(feature = "vk")]
    route: Option<(String, String)>,
    #[cfg(feature = "vk")]
    back: VecDeque<BrowseReturn>,
    #[cfg(feature = "vk")]
    pending_playback: Option<PlaybackJob>,
    #[cfg(feature = "vk")]
    worker: Option<Worker>,
    #[cfg(feature = "vk")]
    client: Option<VkClient>,
    /// Keeps the same authenticated scope available to explicit exports while a worker runs.
    #[cfg(all(
        feature = "vk",
        any(
            feature = "commons-upload",
            feature = "archive-upload",
            feature = "acoustid"
        )
    ))]
    export_client: Option<VkClient>,
    #[cfg(feature = "vk")]
    cookie_file: Option<PathBuf>,
    #[cfg(feature = "vk")]
    cookie_generation: u64,
    #[cfg(feature = "vk")]
    restore: bool,
    /// Survives rendering the empty startup list until the restored first page arrives.
    #[cfg(feature = "vk")]
    restore_selected: Option<usize>,
    #[cfg(feature = "vk")]
    page_turn: Option<usize>,
    #[cfg(feature = "vk")]
    admitting: Option<MediaId>,
    /// Metadata remains process-local, including private source reload ownership.
    #[cfg(feature = "vk")]
    retained: HashMap<VkAudioId, VkTrack>,
    /// Prevents repeated optional lookups for missing titles within one result generation.
    #[cfg(feature = "vk")]
    album_attempted: HashSet<VkAudioId>,
    #[cfg(feature = "vk")]
    capacity: Option<crate::view::SearchPageCapacities>,
}

impl VkState {
    /// Supplies one authenticated, non-networking track for cross-lane controller tests.
    #[cfg(all(
        test,
        feature = "vk",
        any(
            feature = "commons-upload",
            feature = "archive-upload",
            feature = "acoustid"
        )
    ))]
    pub(super) fn export_fixture(track: VkTrack, cookie_file: PathBuf) -> Self {
        Self {
            client: Some(VkClient::fixture_for_track(&track)),
            items: vec![track],
            cookie_file: Some(cookie_file),
            ..Self::default()
        }
    }

    /// Restores only the user's query and row, deferring all authenticated work.
    pub(super) fn restored(saved: &SessionState) -> Self {
        Self {
            query: saved.vk_search_text.clone(),
            selected: saved.vk_selected_row.unwrap_or_default(),
            #[cfg(feature = "vk")]
            restore: !saved.vk_search_text.trim().is_empty(),
            #[cfg(feature = "vk")]
            restore_selected: (!saved.vk_search_text.trim().is_empty())
                .then_some(saved.vk_selected_row.unwrap_or_default()),
            #[cfg(feature = "vk")]
            results_kind: saved.vk_search_kind,
            ..Self::default()
        }
    }
}

/// One imported session's review ownership; private paths never enter the view model.
#[cfg(all(
    feature = "vk",
    any(
        feature = "commons-upload",
        feature = "archive-upload",
        feature = "acoustid"
    )
))]
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct VkExportOwner {
    cookie_file: PathBuf,
    generation: u64,
}

/// One query or continuation whose result belongs to one navigation generation.
#[cfg(feature = "vk")]
#[derive(Clone)]
struct SearchJob {
    generation: u64,
    query: crate::providers::vk::VkQuery,
    kind: VkSearchKind,
    cursor: Option<VkPageCursor>,
}

/// Catalogue kind selected explicitly from the current track's advertised metadata.
#[cfg(feature = "vk")]
#[derive(Clone, Copy)]
enum BrowseKind {
    Album,
    Artist,
}

/// Exact scoped provider result retained while a catalogue request is queued.
#[cfg(feature = "vk")]
#[derive(Clone)]
struct BrowseJob {
    generation: u64,
    source: BrowseSource,
}

/// Album search results and track metadata retain their own provider request references.
#[cfg(feature = "vk")]
#[derive(Clone)]
enum BrowseSource {
    Track {
        track: Box<VkTrack>,
        kind: BrowseKind,
    },
    Album(Box<VkAlbum>),
}

/// Bounded, process-local return location for artist/album browsing.
#[cfg(feature = "vk")]
#[derive(Clone)]
struct BrowseReturn {
    page: VkNavigationSnapshot,
    query: String,
    selected: usize,
}

/// Canonical playback intent, independent of subsequent catalogue navigation.
#[cfg(feature = "vk")]
#[derive(Clone)]
struct PlaybackJob {
    generation: u64,
    item: QueueItem,
    queued: bool,
    origin: Option<AutoplayOrigin>,
}

/// Optional metadata belongs to the selected result's navigation generation.
#[cfg(feature = "vk")]
#[derive(Clone)]
struct AlbumJob {
    generation: u64,
    track: Box<VkTrack>,
}

/// Source operations share one authenticated lane, with explicit requests first.
#[cfg(feature = "vk")]
#[derive(Clone)]
enum Job {
    Search(SearchJob),
    Browse(BrowseJob),
    Playback(Box<PlaybackJob>),
    Album(AlbumJob),
}

/// A source response never exposes private session state to a frontend projection.
#[cfg(feature = "vk")]
enum Reply {
    Page(VkPage),
    Albums(VkAlbumPage),
    Playback(Box<VkTrack>, VkPlaybackUrl),
    Album(Option<String>),
}

/// Returns the client without copying credentials into each subsequent worker.
#[cfg(feature = "vk")]
struct Completion {
    client: Option<VkClient>,
    result: Result<Reply, String>,
}

/// One in-flight authenticated request remains owned until its final publication.
#[cfg(feature = "vk")]
struct Worker {
    job: Job,
    cookie_file: PathBuf,
    cookie_generation: u64,
    response: Receiver<Completion>,
    thread: JoinHandle<()>,
}

/// Process-local public catalogue location used by reversible now-playing navigation.
#[cfg(feature = "vk")]
#[derive(Clone)]
pub(super) struct VkNavigationSnapshot {
    search_kind: VkSearchKind,
    results_kind: VkSearchKind,
    albums: Vec<VkAlbum>,
    items: Vec<VkTrack>,
    next: Option<VkPageCursor>,
    route: Option<(String, String)>,
    back: VecDeque<BrowseReturn>,
}

impl AppController {
    /// Reports the next explicit search's viewport without changing an existing cursor.
    #[cfg(feature = "vk")]
    pub(super) fn update_vk_search_page_capacities(
        &mut self,
        capacities: crate::view::SearchPageCapacities,
    ) {
        self.vk.capacity = Some(capacities.clamped(1, 100));
    }

    /// Explains explicit cookie import without discovering a browser or opening a network connection.
    #[cfg(feature = "vk")]
    fn vk_setup_details() -> DetailView {
        DetailView {
            title: "VK Music setup".to_owned(),
            source: "VK".to_owned(),
            description: format!(
                "Export your signed-in vk.ru session as a Netscape cookies.txt file.\n\nSet {} to that file's absolute path, or set providers.vk_cookies_file in secrets/credentials.toml, then restart Youta.\n\nThe file is read for VK catalogue and playback requests. Youta never discovers browser profiles automatically.",
                crate::config::VK_COOKIES_FILE_ENV
            ),
            ..DetailView::default()
        }
    }

    /// Shares only the current catalogue's sanitized failure with frontend-visible projections.
    #[cfg(feature = "vk")]
    fn current_vk_search_error(&self) -> Option<&str> {
        self.vk
            .search_error
            .as_ref()
            .filter(|(generation, _)| {
                *generation == self.vk.generation
                    && self.vk.cookie_file.is_some()
                    && self.vk.cookie_file == self.config.providers.vk_cookies_file
            })
            .map(|(_, error)| error.as_str())
    }

    /// Projects retained rows without starting work merely because the source is visible.
    pub(super) fn populate_vk(&mut self) {
        #[cfg(feature = "vk")]
        {
            self.refresh_vk_rows();
            self.update_vk_detail();
            self.view.vk_back_available = !self.vk.back.is_empty();
            self.view.status_line = if self.config.providers.vk_cookies_file.is_none() {
                "VK requires an explicitly supplied Netscape cookie file".to_owned()
            } else if let Some(error) = self.current_vk_search_error() {
                error.to_owned()
            } else {
                format!(
                    "{} VK {} · / Search · v Tracks/Albums · Enter {}",
                    self.vk_result_count(),
                    self.vk.results_kind.label(),
                    if self.vk.results_kind == VkSearchKind::Albums {
                        "Open"
                    } else {
                        "Play"
                    },
                )
            };
            if self.vk.pending_search.is_some()
                || self.vk.pending_browse.is_some()
                || self
                    .vk
                    .worker
                    .as_ref()
                    .is_some_and(|worker| match &worker.job {
                        Job::Search(job) => job.generation == self.vk.generation,
                        Job::Browse(job) => job.generation == self.vk.generation,
                        Job::Playback(_) | Job::Album(_) => false,
                    })
            {
                self.begin_search_activity(SearchActivity::Vk);
            }
        }
        #[cfg(not(feature = "vk"))]
        {
            self.view.rows.clear();
            self.view.details = None;
            self.view.status_line = "This build omits the `vk` feature".to_owned();
        }
    }

    /// Keeps selection, canonical Details and known album text aligned.
    pub(super) fn update_vk_detail(&mut self) {
        self.vk.selected = self.view.selected;
        #[cfg(feature = "vk")]
        {
            if self
                .vk
                .page_turn
                .is_some_and(|row| row != self.view.selected)
            {
                self.vk.page_turn = None;
            }
            self.view.details = if self.vk.results_kind == VkSearchKind::Albums {
                self.vk.albums.get(self.view.selected).map(|album| {
                    let url = album.webpage_url();
                    let mut description = Vec::new();
                    if let Some(year) = album.year {
                        description.push(year.to_string());
                    }
                    if let Some(count) = album.track_count {
                        description.push(format!(
                            "{count} track{}",
                            if count == 1 { "" } else { "s" }
                        ));
                    }
                    DetailView {
                        title: album.title.clone(),
                        channel_name: album.artist.clone(),
                        source: "VK".to_owned(),
                        webpage_url: Some(url.clone()),
                        description: description.join(" · "),
                        thumbnail_url: album.artwork_url.clone(),
                        thumbnail_dimensions: album.artwork_dimensions,
                        links: vec![DetailLinkView {
                            url: url.to_string(),
                            presentation: DetailLinkPresentation::UrlOnly,
                            ..DetailLinkView::default()
                        }],
                        ..DetailView::default()
                    }
                })
            } else {
                self.vk
                    .items
                    .get(self.view.selected)
                    .map(|track| DetailView {
                        media_id: Some(media_id(track.id)),
                        title: track.title.clone(),
                        channel_name: track.artist.clone(),
                        channel_webpage_url: track.artist_webpage_url(),
                        source: "VK".to_owned(),
                        webpage_url: Some(track.id.webpage_url()),
                        length: track
                            .duration_seconds
                            .map_or_else(String::new, format_seconds),
                        thumbnail_url: track.artwork_url.clone(),
                        thumbnail_dimensions: track.artwork_dimensions,
                        links: track_links(track),
                        ..DetailView::default()
                    })
            };
            if self.view.details.is_none() && self.config.providers.vk_cookies_file.is_none() {
                self.view.details = Some(Self::vk_setup_details());
            } else if self.view.details.is_none()
                && let Some(error) = self.current_vk_search_error()
            {
                // The TUI has no global status row; diagnostics must use the existing Details panel.
                self.view.details = Some(DetailView {
                    title: "VK search failed".to_owned(),
                    source: "VK".to_owned(),
                    description: format!("{error}\n\nPress / to retry the search."),
                    ..DetailView::default()
                });
            }
        }
        #[cfg(not(feature = "vk"))]
        {
            self.view.details = None;
        }
        self.refresh_selected_private_note_state();
        #[cfg(all(feature = "vk", feature = "acoustid"))]
        self.apply_vk_fingerprint_details();
    }

    /// Accepts text, explicit numeric/profile queries or canonical track identities.
    pub(super) fn submit_vk_search(&mut self, query: String) {
        #[cfg(feature = "vk")]
        {
            self.vk.restore = false;
            self.vk.restore_selected = None;
            let parsed = match crate::providers::vk::parse_query(&query) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.view.status_line = error.to_string();
                    return;
                }
            };
            // Explicit profile/audio identities retain their established track-browsing meaning.
            let kind = if matches!(parsed, VkQuery::Search(_)) {
                self.view.vk_search_kind
            } else {
                VkSearchKind::Tracks
            };
            self.view.vk_search_kind = kind;
            self.vk.pending_browse = None;
            self.vk.route = None;
            self.vk.back.clear();
            self.vk.query = query;
            self.vk.search_error = None;
            self.vk.results_kind = kind;
            self.refresh_vk_cookie_owner();
            if self.config.providers.vk_cookies_file.is_none() {
                self.populate_vk();
                return;
            }
            self.vk.generation = self.vk.generation.wrapping_add(1);
            self.vk.selected = 0;
            self.vk.album_attempted.clear();
            self.vk.items.clear();
            self.vk.albums.clear();
            self.vk.next = None;
            self.vk.page_turn = None;
            self.vk.pending_search = Some(SearchJob {
                generation: self.vk.generation,
                query: parsed,
                kind,
                cursor: None,
            });
            self.view.selected = 0;
            self.populate_vk();
            self.begin_search_activity(SearchActivity::Vk);
            "Searching VK music...".clone_into(&mut self.view.status_line);
            self.poll_vk_worker();
        }
        #[cfg(not(feature = "vk"))]
        {
            let _ = query;
            self.populate_vk();
        }
    }

    /// Changes only the VK search category and reuses its existing explicit worker lane.
    pub(super) fn cycle_vk_search_kind(&mut self) {
        if self.view.screen != Screen::Vk {
            return;
        }
        #[cfg(feature = "vk")]
        {
            let query = self.view.search_query.clone();
            if !query.trim().is_empty() {
                if let Err(error) = crate::providers::vk::parse_query(&query) {
                    self.view.status_line = error.to_string();
                    return;
                }
                self.view.vk_search_kind = self.view.vk_search_kind.next();
                self.submit_vk_search(query);
            } else {
                self.view.vk_search_kind = self.view.vk_search_kind.next();
                self.vk.generation = self.vk.generation.wrapping_add(1);
                self.vk.results_kind = self.view.vk_search_kind;
                self.vk.items.clear();
                self.vk.albums.clear();
                self.vk.next = None;
                self.vk.pending_search = None;
                self.vk.pending_browse = None;
                self.vk.route = None;
                self.vk.back.clear();
                self.vk.restore = false;
                self.vk.restore_selected = None;
                self.vk.page_turn = None;
                self.vk.search_error = None;
                self.vk.selected = 0;
                self.vk.query.clear();
                self.finish_search_activity(SearchActivity::Vk);
                self.populate_vk();
            }
            self.refresh_selected_playlist_state();
        }
        #[cfg(not(feature = "vk"))]
        self.populate_vk();
    }

    /// Returns only a canonical public page for the selected track or album.
    pub(super) fn selected_vk_public_url(&self) -> Option<String> {
        #[cfg(feature = "vk")]
        {
            if self.vk.results_kind == VkSearchKind::Albums {
                return self
                    .vk
                    .albums
                    .get(self.view.selected)
                    .map(|album| album.webpage_url().to_string());
            }
            self.selected_vk_queue_item()
                .ok()
                .map(|item| item.media.webpage_url.to_string())
        }
        #[cfg(not(feature = "vk"))]
        None
    }

    /// Saves the root search row, since private nested catalogue references are process-local.
    pub(super) fn vk_session_selection(&self) -> usize {
        #[cfg(feature = "vk")]
        {
            if let Some(parent) = self.vk.back.front() {
                return parent.selected;
            }
            if let Some(selected) = self.vk.restore_selected {
                return selected;
            }
        }
        self.vk.selected
    }

    /// Counts the current catalogue kind without conflating album rows with playable tracks.
    #[cfg(feature = "vk")]
    fn vk_result_count(&self) -> usize {
        match self.vk.results_kind {
            VkSearchKind::Tracks => self.vk.items.len(),
            VkSearchKind::Albums => self.vk.albums.len(),
        }
    }

    /// Revokes the previous session's private reload keys when the explicit import changes.
    #[cfg(feature = "vk")]
    fn refresh_vk_cookie_owner(&mut self) {
        if self.vk.cookie_file != self.config.providers.vk_cookies_file {
            #[cfg(feature = "acoustid")]
            self.clear_vk_fingerprint_session();
            self.revoke_vk_now_playing_navigation();
            self.vk
                .cookie_file
                .clone_from(&self.config.providers.vk_cookies_file);
            self.vk.client = None;
            #[cfg(any(
                feature = "commons-upload",
                feature = "archive-upload",
                feature = "acoustid"
            ))]
            {
                self.vk.export_client = None;
            }
            self.vk.cookie_generation = self.vk.cookie_generation.wrapping_add(1);
            self.vk.search_error = None;
            if !self.vk.restore {
                self.vk.restore_selected = None;
            }
            self.vk.retained.clear();
            self.vk.album_attempted.clear();
            self.vk.generation = self.vk.generation.wrapping_add(1);
            self.vk.pending_search = None;
            self.vk.pending_browse = None;
            self.vk.route = None;
            self.vk.back.clear();
            self.vk.items.clear();
            self.vk.albums.clear();
            self.vk.next = None;
            self.cancel_vk_playback();
        }
    }

    /// Activates only real tracks or an explicitly advertised continuation.
    pub(super) fn activate_vk_selection(&mut self) {
        #[cfg(feature = "vk")]
        {
            if self.view.selected == self.vk_result_count() {
                if self.vk.pending_search.is_none()
                    && self.vk.pending_browse.is_none()
                    && self
                        .vk
                        .worker
                        .as_ref()
                        .is_none_or(|worker| matches!(worker.job, Job::Album(_)))
                    && let Some(cursor) = self.vk.next.clone()
                {
                    self.vk.page_turn = Some(self.view.selected);
                    self.vk.pending_search = Some(SearchJob {
                        generation: self.vk.generation,
                        query: VkQuery::Search(self.vk.query.clone()),
                        kind: self.vk.results_kind,
                        cursor: Some(cursor),
                    });
                    self.begin_search_activity(SearchActivity::Vk);
                    self.poll_vk_worker();
                }
                return;
            }
            if self.vk.results_kind == VkSearchKind::Albums {
                self.open_selected_vk_album();
                return;
            }
            match self.selected_vk_queue_item() {
                Ok(item) => self.play_queue_item(item, false),
                Err(error) => self.view.status_line = error,
            }
        }
    }

    /// Builds restart-safe queue metadata; private reload state stays outside this value.
    pub(super) fn selected_vk_queue_item(&self) -> Result<QueueItem, String> {
        #[cfg(feature = "vk")]
        {
            if self.vk.results_kind != VkSearchKind::Tracks {
                return Err("Open a VK album and select a track".to_owned());
            }
            self.vk
                .items
                .get(self.view.selected)
                .map(queue_item)
                .ok_or_else(|| "Select a VK track".to_owned())
        }
        #[cfg(not(feature = "vk"))]
        Err("This build omits the `vk` feature".to_owned())
    }

    /// Opens only the selected track's advertised album, never an arbitrary action URL.
    pub(super) fn open_vk_album(&mut self, url: String) {
        #[cfg(feature = "vk")]
        self.browse_vk_catalogue(url, BrowseKind::Album);
        #[cfg(not(feature = "vk"))]
        let _ = url;
    }

    /// Opens only a provider-advertised artist identity, not a guessed name search.
    pub(super) fn open_vk_artist(&mut self, url: String) {
        #[cfg(feature = "vk")]
        self.browse_vk_catalogue(url, BrowseKind::Artist);
        #[cfg(not(feature = "vk"))]
        let _ = url;
    }

    /// Captures a bounded parent page before starting explicit native catalogue work.
    #[cfg(feature = "vk")]
    fn browse_vk_catalogue(&mut self, url: String, kind: BrowseKind) {
        if self.view.screen != Screen::Vk
            || self.config.providers.vk_cookies_file.is_none()
            || self.vk.results_kind != VkSearchKind::Tracks
        {
            return;
        }
        self.refresh_vk_cookie_owner();
        let Some(track) = self.vk.items.get(self.view.selected).cloned() else {
            return;
        };
        let (expected, title) = match kind {
            BrowseKind::Album => (
                track.album_webpage_url(),
                track.album_title.clone().unwrap_or_else(|| "Album".into()),
            ),
            BrowseKind::Artist => (track.artist_webpage_url(), track.artist.clone()),
        };
        if expected.as_ref().map(url::Url::as_str) != Some(url.as_str())
            || self.vk.route.as_ref().is_some_and(|route| route.0 == url)
            || self
                .view
                .details
                .as_ref()
                .and_then(|details| details.media_id.as_ref())
                != Some(&media_id(track.id))
        {
            return;
        }
        self.begin_vk_browse(
            BrowseSource::Track {
                track: Box::new(track),
                kind,
            },
            url,
            title,
        );
    }

    /// Opens the exact selected album result using its private provider-owned reference.
    #[cfg(feature = "vk")]
    fn open_selected_vk_album(&mut self) {
        if self.view.screen != Screen::Vk || self.config.providers.vk_cookies_file.is_none() {
            return;
        }
        self.refresh_vk_cookie_owner();
        let Some(album) = self.vk.albums.get(self.view.selected).cloned() else {
            return;
        };
        let url = album.webpage_url().to_string();
        let title = album.title.clone();
        self.begin_vk_browse(BrowseSource::Album(Box::new(album)), url, title);
    }

    /// Moves a parent into bounded history, retaining the root search and seven recent parents.
    ///
    /// The root owns the restart-safe search selection even after intermediate pages expire.
    #[cfg(feature = "vk")]
    fn begin_vk_browse(&mut self, source: BrowseSource, url: String, title: String) {
        if self.vk.back.len() == 8 {
            self.vk.back.remove(1);
        }
        self.vk.back.push_back(BrowseReturn {
            page: VkNavigationSnapshot {
                search_kind: self.view.vk_search_kind,
                results_kind: self.vk.results_kind,
                albums: std::mem::take(&mut self.vk.albums),
                items: std::mem::take(&mut self.vk.items),
                next: self.vk.next.take(),
                route: self.vk.route.take(),
                back: VecDeque::new(),
            },
            query: self.vk.query.clone(),
            selected: self.view.selected,
        });
        self.vk.generation = self.vk.generation.wrapping_add(1);
        self.vk.results_kind = VkSearchKind::Tracks;
        self.vk.pending_search = None;
        self.vk.restore = false;
        self.vk.restore_selected = None;
        self.vk.page_turn = None;
        self.vk.album_attempted.clear();
        self.vk.route = Some((url, title));
        self.vk.selected = 0;
        self.vk.pending_browse = Some(BrowseJob {
            generation: self.vk.generation,
            source,
        });
        self.view.selected = 0;
        self.populate_vk();
        self.refresh_selected_playlist_state();
        self.begin_search_activity(SearchActivity::Vk);
        "Loading VK tracks...".clone_into(&mut self.view.status_line);
        if !self.diagnostic_only {
            self.poll_vk_worker();
        }
    }

    /// Returns to cached results and selection without searching again or changing playback.
    #[cfg(feature = "vk")]
    pub(super) fn back_vk_catalogue(&mut self) -> bool {
        self.refresh_vk_cookie_owner();
        let Some(mut previous) = self.vk.back.pop_back() else {
            return false;
        };
        let ancestors = std::mem::take(&mut self.vk.back);
        self.swap_vk_navigation(&mut previous.page);
        self.vk.back = ancestors;
        self.vk.query = previous.query;
        self.view.search_query.clone_from(&self.vk.query);
        self.vk.selected = previous.selected;
        self.view.details_scroll = 0;
        self.view.selected_detail_link = None;
        self.finish_search_activity(SearchActivity::Vk);
        self.populate_vk();
        self.refresh_selected_playlist_state();
        true
    }

    /// Shares the existing compact progress-row projection with other music sources.
    #[cfg(feature = "vk")]
    fn refresh_vk_rows(&mut self) {
        self.view.rows = if self.vk.results_kind == VkSearchKind::Albums {
            self.vk
                .albums
                .iter()
                .map(|album| RowView {
                    title: album.title.clone(),
                    subtitle: album.artist.clone(),
                    source: "VK".to_owned(),
                    thumbnail_url: album.artwork_url.clone(),
                    compact: true,
                    ..RowView::default()
                })
                .collect()
        } else {
            self.vk
                .items
                .iter()
                .map(|track| {
                    let id = media_id(track.id);
                    let progress = self.store.progress(&id).ok().flatten();
                    let playback = PlaybackRowState::from_progress(progress.as_ref());
                    RowView {
                        media_id: Some(id),
                        title: track.title.clone(),
                        subtitle: track.artist.clone(),
                        source: "VK".to_owned(),
                        thumbnail_url: track.artwork_url.clone(),
                        watched_percent: playback.watched_percent,
                        playback_started: playback.playback_started,
                        compact: true,
                        ..RowView::default()
                    }
                })
                .collect()
        };
        if self.vk.next.is_some() {
            self.view.rows.push(RowView {
                title: format!("Load more {}...", self.vk.results_kind.label()),
                compact: true,
                ..RowView::default()
            });
        }
        self.view.selected = self.vk.selected.min(self.view.rows.len().saturating_sub(1));
    }

    /// Applies only current replies, deduplicating bounded continuations without automatic crawling.
    #[cfg(feature = "vk")]
    fn apply_vk_page(&mut self, generation: u64, append: bool, result: Result<VkPage, String>) {
        if generation != self.vk.generation {
            return;
        }
        self.finish_search_activity(SearchActivity::Vk);
        match result {
            Ok(page) => {
                self.vk.search_error = None;
                self.vk.results_kind = VkSearchKind::Tracks;
                self.vk.albums.clear();
                if !append {
                    self.vk.items.clear();
                }
                let previous = self.vk.items.len();
                for track in page.tracks {
                    if self.vk.items.len() >= 1_000 {
                        break;
                    }
                    if !self.vk.items.iter().any(|old| old.id == track.id) {
                        self.retain_vk_track(track.clone());
                        self.vk.items.push(track);
                    }
                }
                self.vk.next = (self.vk.items.len() > previous && self.vk.items.len() < 1_000)
                    .then_some(page.next)
                    .flatten();
                if let Some(selected) = self.vk.restore_selected.take() {
                    self.vk.selected = selected;
                }
                if self
                    .vk
                    .page_turn
                    .take()
                    .is_some_and(|row| self.view.screen == Screen::Vk && self.view.selected == row)
                    && self.vk.items.len() > previous
                {
                    let viewport = self
                        .vk
                        .capacity
                        .map_or(50, |capacity| capacity.for_query(&self.vk.query));
                    self.vk.selected = if self.vk.items.len() - previous > viewport {
                        previous.saturating_add(viewport.saturating_sub(1))
                    } else if self.vk.next.is_some() {
                        self.vk.items.len()
                    } else {
                        self.vk.items.len().saturating_sub(1)
                    };
                }
                if self.view.screen == Screen::Vk {
                    self.populate_vk();
                    self.refresh_selected_playlist_state();
                }
            }
            Err(error) => {
                self.vk.page_turn = None;
                let message = format!("VK: {error}");
                self.vk.search_error = Some((generation, message.clone()));
                if self.view.screen == Screen::Vk {
                    self.update_vk_detail();
                    self.view.status_line = message;
                }
            }
        }
    }

    /// Publishes bounded album results only to their current navigation and imported session.
    #[cfg(feature = "vk")]
    fn apply_vk_album_page(
        &mut self,
        generation: u64,
        append: bool,
        result: Result<VkAlbumPage, String>,
    ) {
        if generation != self.vk.generation {
            return;
        }
        let page = match result {
            Ok(page) => page,
            Err(error) => {
                self.apply_vk_page(generation, append, Err(error));
                return;
            }
        };
        self.finish_search_activity(SearchActivity::Vk);
        self.vk.search_error = None;
        self.vk.results_kind = VkSearchKind::Albums;
        self.vk.items.clear();
        if !append {
            self.vk.albums.clear();
        }
        let previous = self.vk.albums.len();
        for album in page.albums {
            if self.vk.albums.len() >= 1_000 {
                break;
            }
            if !self
                .vk
                .albums
                .iter()
                .any(|old| old.webpage_url() == album.webpage_url())
            {
                self.vk.albums.push(album);
            }
        }
        self.vk.next = (self.vk.albums.len() > previous && self.vk.albums.len() < 1_000)
            .then_some(page.next)
            .flatten();
        if let Some(selected) = self.vk.restore_selected.take() {
            self.vk.selected = selected;
        }
        if self
            .vk
            .page_turn
            .take()
            .is_some_and(|row| self.view.screen == Screen::Vk && self.view.selected == row)
            && self.vk.albums.len() > previous
        {
            let viewport = self
                .vk
                .capacity
                .map_or(50, |capacity| capacity.for_query(&self.vk.query));
            self.vk.selected = if self.vk.albums.len() - previous > viewport {
                previous.saturating_add(viewport.saturating_sub(1))
            } else if self.vk.next.is_some() {
                self.vk.albums.len()
            } else {
                self.vk.albums.len().saturating_sub(1)
            };
        }
        if self.view.screen == Screen::Vk {
            self.populate_vk();
            self.refresh_selected_playlist_state();
        }
    }

    /// Resolves retained exact audio metadata without touching generic VK video entries.
    #[cfg(feature = "vk")]
    pub(super) fn request_vk_playback(
        &mut self,
        item: QueueItem,
        queued: bool,
        origin: Option<AutoplayOrigin>,
    ) {
        self.refresh_vk_cookie_owner();
        self.cancel_vk_playback();
        #[cfg(feature = "funkwhale")]
        self.cancel_funkwhale_playback();
        #[cfg(feature = "soundcloud")]
        self.cancel_pending_soundcloud_playback();
        #[cfg(feature = "yandex-music")]
        self.cancel_pending_yandex_music_playback();
        #[cfg(feature = "yt-dlp")]
        self.cancel_live_start_preparation();
        self.cancel_pending_history_replay();
        if self.pending_playlist_replay.take().is_some() {
            self.supersede_search_generation();
        }
        #[cfg(feature = "tracker-music")]
        {
            self.pending_tracker_preparation = None;
        }
        #[cfg(feature = "bandcamp")]
        self.cancel_pending_bandcamp_resolution();
        #[cfg(feature = "bbc-radio")]
        if self.pending_bbc_playback.take().is_some() {
            self.bbc_playback_generation = self.bbc_playback_generation.wrapping_add(1);
            self.clear_playback_start_activity();
        }
        if self.config.providers.vk_cookies_file.is_none() {
            self.view.status_line = format!(
                "VK requires {} or providers.vk_cookies_file",
                crate::config::VK_COOKIES_FILE_ENV
            );
            return;
        }
        if audio_identity(&item).is_none() {
            "Select a canonical VK audio item".clone_into(&mut self.view.status_line);
            return;
        }
        self.vk.pending_playback = Some(PlaybackJob {
            generation: self.vk.playback_generation,
            item,
            queued,
            origin,
        });
        "Resolving VK audio...".clone_into(&mut self.view.status_line);
        self.poll_vk_worker();
    }

    /// Cancels an intent without interrupting current playback or spawning another HTTP lane.
    #[cfg(feature = "vk")]
    pub(super) fn cancel_vk_playback(&mut self) {
        self.vk.playback_generation = self.vk.playback_generation.wrapping_add(1);
        self.vk.pending_playback = None;
    }

    /// Reports only the current playback owner, not obsolete background completions.
    #[cfg(feature = "vk")]
    pub(super) fn vk_playback_pending(&self) -> bool {
        self.vk.pending_playback.is_some() || self.vk.worker.as_ref().is_some_and(|worker| matches!(&worker.job, Job::Playback(job) if job.generation == self.vk.playback_generation))
    }

    /// Preserves an already-positioned explicit queue transition while another item ends.
    #[cfg(feature = "vk")]
    pub(super) fn vk_playback_owns_queue_cursor(&self) -> bool {
        self.vk
            .pending_playback
            .as_ref()
            .filter(|job| job.generation == self.vk.playback_generation)
            .map(|job| job.queued)
            .or_else(|| {
                self.vk
                    .worker
                    .as_ref()
                    .and_then(|worker| match &worker.job {
                        Job::Playback(job) if job.generation == self.vk.playback_generation => {
                            Some(job.queued)
                        }
                        _ => None,
                    })
            })
            .unwrap_or(false)
    }

    /// Admits a resolved input only during the matching provider completion.
    #[cfg(feature = "vk")]
    pub(super) fn vk_resolved_candidate(&self, id: &MediaId) -> bool {
        self.vk.admitting.as_ref() == Some(id)
    }

    /// Drains published replies and starts one pending request, prioritizing explicit playback.
    pub(super) fn poll_vk_worker(&mut self) {
        #[cfg(feature = "vk")]
        {
            if self.view.quitting || self.shutdown_persistence_succeeded.is_some() {
                return;
            }
            if self.vk.cookie_file != self.config.providers.vk_cookies_file {
                self.refresh_vk_cookie_owner();
                if self.view.screen == Screen::Vk {
                    self.populate_vk();
                }
            }
            if !self.config.persistence.save_playback_history {
                self.vk.restore = false;
                self.vk.restore_selected = None;
            }
            if self.view.screen == Screen::Vk && self.vk.restore && !self.view.search_editing {
                self.vk.restore = false;
                let selected = self.vk.restore_selected.take().unwrap_or(self.vk.selected);
                self.submit_vk_search(self.vk.query.clone());
                self.vk.selected = selected;
                self.vk.restore_selected = Some(selected);
                return;
            }
            let completed =
                self.vk
                    .worker
                    .as_ref()
                    .and_then(|worker| match worker.response.try_recv() {
                        Ok(reply) => Some(reply),
                        Err(TryRecvError::Disconnected) => Some(Completion {
                            client: None,
                            result: Err("VK worker stopped without a result".to_owned()),
                        }),
                        Err(TryRecvError::Empty) => None,
                    });
            if let Some(completion) = completed {
                let worker = self.vk.worker.take().expect("published VK worker");
                worker_notifications::reap_published_worker(worker.thread);
                if self.config.providers.vk_cookies_file.as_ref() == Some(&worker.cookie_file)
                    && self.vk.cookie_generation == worker.cookie_generation
                {
                    #[cfg(any(
                        feature = "commons-upload",
                        feature = "archive-upload",
                        feature = "acoustid"
                    ))]
                    {
                        self.vk.export_client = completion.client.clone();
                    }
                    self.vk.client = completion.client;
                    match worker.job {
                        Job::Search(job) if job.kind == VkSearchKind::Albums => self
                            .apply_vk_album_page(
                                job.generation,
                                job.cursor.is_some(),
                                completion.result.and_then(|reply| match reply {
                                    Reply::Albums(page) => Ok(page),
                                    _ => Err("VK worker returned the wrong response".to_owned()),
                                }),
                            ),
                        Job::Search(job) => self.apply_vk_page(
                            job.generation,
                            job.cursor.is_some(),
                            completion.result.and_then(|reply| match reply {
                                Reply::Page(page) => Ok(page),
                                Reply::Albums(_) | Reply::Playback(..) | Reply::Album(_) => {
                                    Err("VK worker returned the wrong response".to_owned())
                                }
                            }),
                        ),
                        Job::Browse(job) => self.apply_vk_page(
                            job.generation,
                            false,
                            completion.result.and_then(|reply| match reply {
                                Reply::Page(page) => Ok(page),
                                _ => Err("VK worker returned the wrong response".to_owned()),
                            }),
                        ),
                        Job::Playback(job) if job.generation == self.vk.playback_generation => {
                            self.apply_vk_playback(*job, completion.result);
                        }
                        Job::Playback(_) => {}
                        Job::Album(job) => self.apply_vk_album(job, completion.result),
                    }
                }
            }
            if self.vk.worker.is_some() {
                return;
            }
            let job = if let Some(job) = self.vk.pending_playback.take() {
                Job::Playback(Box::new(job))
            } else if let Some(job) = self.vk.pending_search.take() {
                Job::Search(job)
            } else if let Some(job) = self.vk.pending_browse.take() {
                Job::Browse(job)
            } else if let Some(job) = self.selected_vk_album_job() {
                self.vk.album_attempted.insert(job.track.id);
                Job::Album(job)
            } else {
                return;
            };
            let Some(path) = self.config.providers.vk_cookies_file.clone() else {
                return;
            };
            let track = match &job {
                Job::Playback(job) => audio_identity(&job.item)
                    .and_then(|id| {
                        self.vk
                            .retained
                            .get(&id)
                            .or_else(|| self.vk.items.iter().find(|track| track.id == id))
                    })
                    .cloned(),
                Job::Search(_) | Job::Browse(_) | Job::Album(_) => None,
            };
            let page_size = self
                .vk
                .capacity
                .map_or(50, |capacity| capacity.for_query(&self.vk.query));
            let client = self.take_vk_worker_client();
            let work = job.clone();
            let input = path.clone();
            let (sender, response) = bounded(1);
            let sender = ResponseSender::new(sender, self.worker_notifier.clone());
            if let Ok(thread) =
                thread::Builder::new()
                    .name("youta-vk".to_owned())
                    .spawn(move || {
                        let _ = sender.send(run_job(work, client, &input, page_size, track));
                    })
            {
                self.vk.worker = Some(Worker {
                    job,
                    cookie_file: path,
                    cookie_generation: self.vk.cookie_generation,
                    response,
                    thread,
                });
            } else if !matches!(job, Job::Album(_)) {
                self.finish_search_activity(SearchActivity::Vk);
                "VK worker could not start".clone_into(&mut self.view.status_line);
            }
        }
    }

    /// Moves the ordinary worker client while retaining its immutable authenticated scope.
    #[cfg(feature = "vk")]
    fn take_vk_worker_client(&mut self) -> Option<VkClient> {
        #[cfg(any(
            feature = "commons-upload",
            feature = "archive-upload",
            feature = "acoustid"
        ))]
        {
            self.vk.export_client = self.vk.client.clone();
        }
        self.vk.client.take()
    }

    /// Captures the currently imported owner without reading credentials or starting work.
    #[cfg(all(
        feature = "vk",
        any(
            feature = "commons-upload",
            feature = "archive-upload",
            feature = "acoustid"
        )
    ))]
    pub(super) fn vk_export_owner(&self) -> Option<VkExportOwner> {
        let cookie_file = self.config.providers.vk_cookies_file.as_ref()?;
        if self.vk.cookie_file.as_ref() != Some(cookie_file)
            || !cookie_file.is_absolute()
            || !self
                .vk
                .client
                .as_ref()
                .or(self.vk.export_client.as_ref())?
                .is_authenticated()
        {
            return None;
        }
        Some(VkExportOwner {
            cookie_file: cookie_file.clone(),
            generation: self.vk.cookie_generation,
        })
    }

    /// Rejects old reviews after a cookie path changes away and back to the same value.
    #[cfg(all(
        feature = "vk",
        any(
            feature = "commons-upload",
            feature = "archive-upload",
            feature = "acoustid"
        )
    ))]
    #[cfg(any(test, feature = "commons-upload", feature = "archive-upload"))]
    pub(super) fn vk_export_owner_is_current(&self, owner: &VkExportOwner) -> bool {
        self.vk_export_owner().as_ref() == Some(owner)
    }

    /// Retains the exact selected/queued provider record rather than searching by its labels.
    #[cfg(all(
        feature = "vk",
        any(
            feature = "commons-upload",
            feature = "archive-upload",
            feature = "acoustid"
        )
    ))]
    pub(super) fn capture_vk_export_source(
        &self,
        item: &QueueItem,
    ) -> Option<crate::archive_upload_media::VkExportSource> {
        self.vk_export_owner()?;
        if !export_supported(item) {
            return None;
        }
        let id = audio_identity(item)?;
        let track = self
            .vk
            .items
            .iter()
            .find(|track| track.id == id)
            .or_else(|| self.vk.retained.get(&id))?;
        let client = self.vk.client.as_ref().or(self.vk.export_client.as_ref())?;
        Some(crate::archive_upload_media::VkExportSource::new(
            client.clone(),
            track.clone(),
        ))
    }

    /// Chooses only visible, unresolved album metadata after explicit work is drained.
    #[cfg(feature = "vk")]
    fn selected_vk_album_job(&self) -> Option<AlbumJob> {
        if self.diagnostic_only
            || self.view.screen != Screen::Vk
            || self.vk.results_kind != VkSearchKind::Tracks
            || self.view.search_editing
            || self.config.providers.vk_cookies_file.is_none()
        {
            return None;
        }
        let track = self.vk.items.get(self.view.selected)?;
        (track.has_album()
            && track.album_title.is_none()
            && !self.vk.album_attempted.contains(&track.id))
        .then(|| AlbumJob {
            generation: self.vk.generation,
            track: Box::new(track.clone()),
        })
    }

    /// Enriches only the matching result generation and leaves optional failures silent.
    #[cfg(feature = "vk")]
    fn apply_vk_album(&mut self, job: AlbumJob, result: Result<Reply, String>) {
        if job.generation != self.vk.generation {
            return;
        }
        let Ok(Reply::Album(Some(title))) = result else {
            return;
        };
        let id = job.track.id;
        for track in self.vk.items.iter_mut().filter(|track| track.id == id) {
            track.album_title = Some(title.clone());
        }
        if let Some(track) = self.vk.retained.get_mut(&id) {
            track.album_title = Some(title);
        }
        if self.view.screen == Screen::Vk
            && self
                .vk
                .items
                .get(self.view.selected)
                .is_some_and(|track| track.id == id)
        {
            self.update_vk_detail();
        }
    }

    /// Sends only the freshly resolved locator to the player, never to persisted metadata.
    #[cfg(feature = "vk")]
    fn apply_vk_playback(&mut self, job: PlaybackJob, result: Result<Reply, String>) {
        let (track, location) = match result {
            Ok(Reply::Playback(track, location)) if media_id(track.id) == job.item.media.id => {
                (*track, location)
            }
            Ok(_) => {
                "VK returned a different track".clone_into(&mut self.view.status_line);
                return;
            }
            Err(error) => {
                self.view.status_line = format!("VK: {error}");
                return;
            }
        };
        let mut item = queue_item(&track);
        item.added_at = job.item.added_at;
        item.start_at_seconds = job.item.start_at_seconds;
        self.retain_vk_track(track);
        let mut input = PlaybackInput::new(location.into_url().to_string());
        input.bypass_ytdl = true;
        self.vk.admitting = Some(item.media.id.clone());
        self.play_queue_item_with_origin_and_input(item, job.queued, job.origin, Some(input));
        self.vk.admitting = None;
    }

    /// Revokes queued work on shutdown; any bounded in-flight request loses publication ownership.
    pub(super) fn shutdown_vk(&mut self) {
        #[cfg(feature = "vk")]
        {
            self.cancel_vk_playback();
            self.vk.generation = self.vk.generation.wrapping_add(1);
            self.vk.pending_search = None;
            self.vk.pending_browse = None;
            self.vk.restore = false;
            self.vk.restore_selected = None;
            self.vk.client = None;
            #[cfg(any(
                feature = "commons-upload",
                feature = "archive-upload",
                feature = "acoustid"
            ))]
            {
                self.vk.export_client = None;
            }
        }
    }

    /// Preserves edited text and the row while a different top-level source is visible.
    pub(super) fn leave_vk_screen(&mut self) {
        self.vk.query.clone_from(&self.view.search_query);
        self.vk.selected = self.view.selected;
        #[cfg(feature = "vk")]
        {
            self.vk.page_turn = None;
        }
        self.finish_search_activity(SearchActivity::Vk);
    }

    /// Captures canonical same-source playback order independently of later searches.
    #[cfg(feature = "vk")]
    pub(super) fn vk_autoplay_origin(&self, id: &MediaId) -> Option<AutoplayOrigin> {
        if self.vk.results_kind != VkSearchKind::Tracks {
            return None;
        }
        let items = self.vk.items.iter().map(queue_item).collect::<Vec<_>>();
        let index = items.iter().position(|item| &item.media.id == id)?;
        Some(AutoplayOrigin::Vk {
            items: items.into(),
            index,
        })
    }

    /// Binds reversible now-playing navigation to the current source catalogue.
    #[cfg(feature = "vk")]
    pub(super) fn vk_navigation_generation(&self) -> u64 {
        self.vk.generation
    }

    /// Retains only one exact playing source row for the shared now-playing navigation.
    #[cfg(feature = "vk")]
    pub(super) fn vk_navigation_snapshot(&self, item: &QueueItem) -> Option<VkNavigationSnapshot> {
        if self.vk.cookie_file != self.config.providers.vk_cookies_file {
            return None;
        }
        let id = audio_identity(item)?;
        let track = self
            .vk
            .retained
            .get(&id)
            .or_else(|| self.vk.items.iter().find(|track| track.id == id))?
            .clone();
        Some(VkNavigationSnapshot {
            search_kind: VkSearchKind::Tracks,
            results_kind: VkSearchKind::Tracks,
            albums: Vec::new(),
            items: vec![track],
            next: None,
            route: None,
            back: VecDeque::new(),
        })
    }

    /// Reversibly swaps real provider rows without replaying media or fetching a catalogue.
    #[cfg(feature = "vk")]
    pub(super) fn swap_vk_navigation(&mut self, snapshot: &mut VkNavigationSnapshot) {
        self.vk.generation = self.vk.generation.wrapping_add(1);
        self.vk.album_attempted.clear();
        self.vk.pending_search = None;
        self.vk.pending_browse = None;
        self.vk.page_turn = None;
        self.vk.restore = false;
        self.vk.restore_selected = None;
        std::mem::swap(&mut self.view.vk_search_kind, &mut snapshot.search_kind);
        std::mem::swap(&mut self.vk.results_kind, &mut snapshot.results_kind);
        std::mem::swap(&mut self.vk.albums, &mut snapshot.albums);
        std::mem::swap(&mut self.vk.items, &mut snapshot.items);
        std::mem::swap(&mut self.vk.next, &mut snapshot.next);
        std::mem::swap(&mut self.vk.route, &mut snapshot.route);
        std::mem::swap(&mut self.vk.back, &mut snapshot.back);
    }

    /// Reveals an existing exact row before falling back to the retained playing snapshot.
    #[cfg(feature = "vk")]
    pub(super) fn reveal_vk_now_playing(&mut self, id: &MediaId) -> bool {
        self.refresh_vk_cookie_owner();
        if self.vk.results_kind != VkSearchKind::Tracks {
            return false;
        }
        let Some(index) = self
            .vk
            .items
            .iter()
            .position(|track| media_id(track.id) == *id)
        else {
            return false;
        };
        self.show_screen(Screen::Vk);
        self.vk.selected = index;
        self.view.selected = index;
        self.update_vk_detail();
        true
    }

    /// Bounds private replay metadata, retaining the accepted track across a cache trim.
    #[cfg(feature = "vk")]
    fn retain_vk_track(&mut self, track: VkTrack) {
        if self.vk.retained.len() >= 2_000 && !self.vk.retained.contains_key(&track.id) {
            let active = self
                .current_media
                .as_ref()
                .filter(|id| id.source == SourceKind::Vk)
                .and_then(|id| VkAudioId::parse(&id.external_id).ok())
                .and_then(|id| self.vk.retained.remove(&id));
            self.vk.retained.clear();
            if let Some(active) = active {
                self.vk.retained.insert(active.id, active);
            }
        }
        self.vk.retained.insert(track.id, track);
    }
}

/// Only an exact canonical audio identity can select the native VK resolver.
#[cfg(feature = "vk")]
pub(super) fn audio_identity(item: &QueueItem) -> Option<VkAudioId> {
    if item.media.id.source != SourceKind::Vk || item.media.kind != MediaKind::Audio {
        return None;
    }
    let id = VkAudioId::parse(&item.media.id.external_id).ok()?;
    (VkAudioId::from_page_url(&item.media.webpage_url).ok()? == id).then_some(id)
}

/// Runs one bounded, authenticated source operation entirely outside the frontend thread.
#[cfg(feature = "vk")]
fn run_job(
    job: Job,
    mut client: Option<VkClient>,
    cookie_file: &Path,
    page_size: usize,
    retained_track: Option<VkTrack>,
) -> Completion {
    let result = (|| {
        if client.is_none() {
            let mut imported =
                VkClient::from_cookie_file(cookie_file).map_err(|error| error.to_string())?;
            imported.authenticate().map_err(|error| error.to_string())?;
            client = Some(imported);
        }
        client
            .as_mut()
            .expect("authenticated VK client")
            .set_page_size(page_size);
        let client = client.as_mut().expect("authenticated VK client");
        match job {
            Job::Browse(job) => match job.source {
                BrowseSource::Track {
                    track,
                    kind: BrowseKind::Album,
                } => client.album_tracks(&track),
                BrowseSource::Track {
                    track,
                    kind: BrowseKind::Artist,
                } => client.artist_tracks(&track),
                BrowseSource::Album(album) => client.album_tracks_from_result(&album),
            }
            .map(Reply::Page)
            .map_err(|error| error.to_string()),
            Job::Album(job) => client
                .album_title(&job.track)
                .map(Reply::Album)
                .map_err(|error| error.to_string()),
            Job::Search(job) => {
                if job.kind == VkSearchKind::Albums {
                    let page = if let Some(cursor) = job.cursor {
                        client.next_album_search_page(&cursor)
                    } else if let VkQuery::Search(query) = job.query {
                        client.search_albums(&query)
                    } else {
                        return Err("Album search requires an album name".to_owned());
                    };
                    return page.map(Reply::Albums).map_err(|error| error.to_string());
                }
                let page = if let Some(cursor) = job.cursor {
                    client
                        .next_page(&cursor)
                        .map_err(|error| error.to_string())?
                } else {
                    match job.query {
                        VkQuery::Search(query) => {
                            client.search(&query).map_err(|error| error.to_string())?
                        }
                        VkQuery::Profile(profile) => client
                            .profile_tracks(&profile)
                            .map_err(|error| error.to_string())?,
                        VkQuery::Track(id) => {
                            if id.owner_id() <= 0 {
                                return Err("Search for this group's VK track; direct group catalogue lookup is not supported".to_owned());
                            }
                            let profile = crate::providers::vk::ProfileReference::parse(
                                &id.owner_id().to_string(),
                            )
                            .map_err(|error| error.to_string())?;
                            let page = client
                                .profile_tracks(&profile)
                                .map_err(|error| error.to_string())?;
                            VkPage {
                                tracks: vec![exact_track(page, id)?],
                                next: None,
                            }
                        }
                    }
                };
                Ok(Reply::Page(page))
            }
            Job::Playback(job) => {
                let track = if let Some(track) = retained_track {
                    track
                } else {
                    let id = audio_identity(&job.item)
                        .ok_or_else(|| "Select a canonical VK audio item".to_owned())?;
                    let query = format!(
                        "{} {}",
                        job.item.media.title,
                        job.item.media.creator.as_deref().unwrap_or_default()
                    );
                    let query = query.chars().take(256).collect::<String>();
                    let page = client.search(&query).map_err(|error| error.to_string())?;
                    exact_track(page, id)?
                };
                let url = client.resolve(&track).map_err(|error| error.to_string())?;
                Ok(Reply::Playback(Box::new(track), url))
            }
        }
    })();
    Completion { client, result }
}

/// Uses the stable owner/audio pair rather than an expiring media URL as the progress key.
#[cfg(feature = "vk")]
fn media_id(id: VkAudioId) -> MediaId {
    MediaId::new(SourceKind::Vk, id.to_string())
}

/// Export controls accept only one finite canonical VK audio record.
#[cfg(any(
    feature = "commons-upload",
    feature = "archive-upload",
    all(feature = "vk", feature = "acoustid")
))]
pub(super) fn export_supported(item: &QueueItem) -> bool {
    #[cfg(feature = "vk")]
    {
        audio_identity(item).is_some() && item.playback_location == item.media.webpage_url.as_str()
    }
    #[cfg(not(feature = "vk"))]
    {
        let _ = item;
        false
    }
}

/// A single bounded lookup may refresh replay metadata only for the exact saved identity.
#[cfg(feature = "vk")]
fn exact_track(page: VkPage, id: VkAudioId) -> Result<VkTrack, String> {
    page.tracks
        .into_iter()
        .find(|track| track.id == id)
        .ok_or_else(|| {
            "This VK track was not found on the first catalogue page; search for it again"
                .to_owned()
        })
}

/// Builds a canonical replay snapshot with no provider-private reload or media fields.
#[cfg(feature = "vk")]
fn queue_item(track: &VkTrack) -> QueueItem {
    let page = track.id.webpage_url();
    QueueItem {
        media: MediaItem {
            id: media_id(track.id),
            kind: MediaKind::Audio,
            title: track.title.clone(),
            creator: Some(track.artist.clone()),
            description: None,
            webpage_url: page.clone(),
            thumbnail_url: track.artwork_url.clone(),
            duration_seconds: track.duration_seconds,
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: Vec::new(),
        },
        playback_location: page.to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    }
}

#[cfg(all(test, feature = "vk"))]
mod tests;

#[cfg(all(test, feature = "vk"))]
mod cross_provider_tests;

#[cfg(all(test, feature = "vk"))]
mod album_tests;

#[cfg(all(test, feature = "vk"))]
mod album_search_tests;

#[cfg(all(test, feature = "vk"))]
mod browse_tests;

#[cfg(all(
    test,
    feature = "vk",
    any(
        feature = "commons-upload",
        feature = "archive-upload",
        feature = "acoustid"
    )
))]
mod export_tests;

/// Places the public track URL first and exposes only validated public catalogue links.
#[cfg(feature = "vk")]
fn track_links(track: &VkTrack) -> Vec<DetailLinkView> {
    let mut links = vec![DetailLinkView {
        url: track.id.webpage_url().to_string(),
        presentation: DetailLinkPresentation::UrlOnly,
        ..DetailLinkView::default()
    }];
    if let Some(url) = track.artist_webpage_url() {
        links.push(DetailLinkView {
            label: track.artist.clone(),
            url: url.to_string(),
            internal_target: Some(DetailLinkInternalTarget::VkArtist(url.to_string())),
            ..DetailLinkView::default()
        });
    }
    if let Some(url) = track.album_webpage_url() {
        links.push(DetailLinkView {
            label: track
                .album_title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or("Album")
                .to_owned(),
            url: url.to_string(),
            internal_target: Some(DetailLinkInternalTarget::VkAlbum(url.to_string())),
            ..DetailLinkView::default()
        });
    }
    links
}
