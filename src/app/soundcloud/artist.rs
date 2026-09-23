//! Bounded public artist catalogues, ordered album slots, and cached Back navigation.

use super::*;

#[cfg(feature = "soundcloud")]
use crate::providers::soundcloak::{
    SoundcloakAlbum, SoundcloakAlbumDetails, SoundcloakAlbumTrack, SoundcloakArtist,
    SoundcloakCatalogCursor,
};

/// Conservative retained-metadata budget, independent of the eight-location cap.
#[cfg(feature = "soundcloud")]
const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;

/// Runtime-only catalogue ownership; never persisted as expiring metadata.
#[cfg(feature = "soundcloud")]
#[derive(Default)]
pub(super) struct SoundCloudCatalogState {
    route: Route,
    history: VecDeque<Snapshot>,
    generation: u64,
    pending: Option<Job>,
    worker: Option<Worker>,
    page_turn: Option<(u64, usize)>,
}

/// Typed rows retain album holes and opaque cursor ownership without inventing track URLs.
#[cfg(feature = "soundcloud")]
#[derive(Clone, Debug, Default)]
enum Route {
    #[default]
    Search,
    Loading,
    Tracks {
        artist: SoundcloakArtist,
        next: Option<SoundcloakCatalogCursor>,
    },
    Albums {
        artist: SoundcloakArtist,
        items: Vec<SoundcloakAlbum>,
        next: Option<SoundcloakCatalogCursor>,
    },
    Album {
        details: Arc<SoundcloakAlbumDetails>,
        slots: Vec<SoundcloakAlbumTrack>,
        next: Option<usize>,
    },
}

/// A small Back stack contains only public metadata and exact navigation positions.
#[cfg(feature = "soundcloud")]
#[derive(Clone)]
struct Snapshot {
    route: Route,
    query: String,
    submitted_query: String,
    tag: Option<String>,
    items: Vec<SoundcloakTrack>,
    selected: usize,
    next_page: Option<u32>,
    query_urn: Option<String>,
    page_limit: Option<usize>,
    /// Conservative preflight weight; shared album metadata is counted in full.
    retained_bytes: usize,
}

/// One catalogue intent replaces any older unsent intent.
#[cfg(feature = "soundcloud")]
#[derive(Clone)]
struct Job {
    generation: u64,
    limit: usize,
    append: bool,
    request: Request,
}

#[cfg(feature = "soundcloud")]
#[derive(Clone)]
enum Request {
    Artist {
        url: url::Url,
        albums: bool,
    },
    Tracks {
        artist: SoundcloakArtist,
        cursor: SoundcloakCatalogCursor,
    },
    Albums {
        artist: SoundcloakArtist,
        cursor: SoundcloakCatalogCursor,
    },
    Album {
        url: url::Url,
    },
    AlbumPage {
        details: Arc<SoundcloakAlbumDetails>,
        offset: usize,
    },
}

#[cfg(feature = "soundcloud")]
struct Page {
    route: Route,
    tracks: Vec<SoundcloakTrack>,
}

#[cfg(feature = "soundcloud")]
struct Worker {
    job: Job,
    response: Receiver<Result<Page, String>>,
    thread: JoinHandle<()>,
}

impl AppController {
    /// Opens a public artist catalogue without requiring an external browser.
    pub(in crate::app) fn open_soundcloud_artist(&mut self, url: String, albums: bool) {
        #[cfg(feature = "soundcloud")]
        {
            // Actions can be dispatched independently of a rendered link. Validate locally
            // before changing navigation or allowing a worker to contact the instance.
            let validated = url::Url::parse(&url)
                .map_err(|error| error.to_string())
                .and_then(|url| {
                    self.soundcloak_client()?
                        .profile_page_url(&url)
                        .map_err(|error| error.to_string())?;
                    Ok(url)
                });
            match validated {
                Ok(url) => self.open_soundcloud_catalog(Request::Artist { url, albums }),
                Err(error) => self.view.status_line = format!("SoundCloud artist: {error}"),
            }
        }
        #[cfg(not(feature = "soundcloud"))]
        let _ = (url, albums);
    }

    /// Routes an exact current SoundCloud comment author into the artist catalogue.
    pub(in crate::app) fn open_soundcloud_comment_author(&mut self, index: usize) {
        #[cfg(feature = "soundcloud")]
        {
            if self.view.screen != Screen::SoundCloud {
                return;
            }
            let Some(track) = self.selected_soundcloud_track() else {
                return;
            };
            let Some(popup) = self.view.video_comments_popup.as_ref().filter(|popup| {
                popup.source == SourceKind::SoundCloud
                    && popup.state == VideoCommentsPopupState::Ready
                    && popup.video_id == track.webpage_url.as_str()
            }) else {
                return;
            };
            let Some(url) = popup
                .comments
                .get(index)
                .and_then(|comment| comment.author_url.clone())
            else {
                return;
            };
            if url::Url::parse(&url)
                .ok()
                .and_then(|url| self.soundcloak_client().ok()?.profile_page_url(&url).ok())
                .is_none()
            {
                return;
            }
            self.invalidate_youtube_video_comments_popup();
            self.open_soundcloud_artist(url, false);
        }
        #[cfg(not(feature = "soundcloud"))]
        let _ = index;
    }

    /// Searches a public tag while retaining its previous catalogue location.
    pub(in crate::app) fn search_soundcloud_tag(&mut self, tag: String) {
        #[cfg(feature = "soundcloud")]
        {
            if self
                .soundcloak_client()
                .and_then(|client| client.genre_url(&tag).map_err(|error| error.to_string()))
                .is_err()
            {
                return;
            }
            self.push_soundcloud_snapshot();
            self.reset_soundcloud_catalog(false);
            self.soundcloud.tag = Some(tag.clone());
            self.soundcloud.query.clone_from(&tag);
            self.show_screen(Screen::SoundCloud);
            self.view.search_query.clone_from(&tag);
            self.start_soundcloud_search(tag);
        }
        #[cfg(not(feature = "soundcloud"))]
        let _ = tag;
    }
}

#[cfg(feature = "soundcloud")]
impl AppController {
    /// Retrieves the selected real track, accounting for unavailable ordered album slots.
    pub(in crate::app) fn selected_soundcloud_track(&self) -> Option<&SoundcloakTrack> {
        match &self.soundcloud.catalog.route {
            Route::Album { slots, .. } => slots.get(self.view.selected)?.track.as_ref(),
            Route::Albums { .. } | Route::Loading => None,
            _ => self.soundcloud.items.get(self.view.selected),
        }
    }

    /// Invalidates responses without killing an in-flight bounded HTTP request.
    pub(super) fn reset_soundcloud_catalog(&mut self, clear_history: bool) {
        let state = &mut self.soundcloud.catalog;
        state.generation = state.generation.wrapping_add(1);
        state.pending = None;
        state.page_turn = None;
        state.route = Route::Search;
        if clear_history {
            state.history.clear();
        }
        self.view.soundcloud_back_available = !state.history.is_empty();
    }

    fn soundcloud_snapshot(&self) -> Snapshot {
        Snapshot {
            route: self.soundcloud.catalog.route.clone(),
            query: self.soundcloud.query.clone(),
            submitted_query: self.soundcloud.submitted_query.clone(),
            tag: self.soundcloud.tag.clone(),
            items: self.soundcloud.items.clone(),
            selected: self.soundcloud.selected,
            next_page: self.soundcloud.next_page,
            query_urn: self.soundcloud.query_urn.clone(),
            page_limit: self.soundcloud.page_limit,
            retained_bytes: 0,
        }
    }

    fn push_soundcloud_snapshot(&mut self) {
        if !matches!(self.soundcloud.catalog.route, Route::Loading) {
            if self.view.screen == Screen::SoundCloud {
                self.soundcloud.selected = self.view.selected;
            }
            // Reject before cloning large descriptions; an oversized destination
            // must not displace a nearer previously retained usable location.
            let Some(weight) = self.soundcloud_history_weight() else {
                return;
            };
            let history = &mut self.soundcloud.catalog.history;
            let mut retained = history
                .iter()
                .map(|snapshot| snapshot.retained_bytes)
                .sum::<usize>();
            while history.len() >= 8 || retained > MAX_HISTORY_BYTES - weight {
                if let Some(oldest) = history.pop_front() {
                    retained -= oldest.retained_bytes;
                } else {
                    break;
                }
            }
            let mut snapshot = self.soundcloud_snapshot();
            snapshot.retained_bytes = weight;
            self.soundcloud.catalog.history.push_back(snapshot);
        }
    }

    /// Estimates borrowed metadata before cloning, with a fourfold allocation margin.
    ///
    /// This is a conservative retention policy, not exact allocator accounting.
    /// The bounded writer retains no formatted text and stops on its first limit
    /// breach. Derived Debug visits opaque cursor data and complete shared album
    /// metadata, including track bodies also duplicated in the visible item list.
    fn soundcloud_history_weight(&self) -> Option<usize> {
        let mut bytes = HistoryBytes(std::mem::size_of::<Snapshot>());
        bytes.rows::<SoundcloakTrack>(self.soundcloud.items.len())?;
        match &self.soundcloud.catalog.route {
            Route::Albums { items, .. } => bytes.rows::<SoundcloakAlbum>(items.len())?,
            Route::Album { details, slots, .. } => {
                bytes.rows::<SoundcloakAlbumDetails>(1)?;
                bytes.rows::<SoundcloakAlbumTrack>(details.tracks.capacity())?;
                bytes.rows::<SoundcloakAlbumTrack>(slots.len())?;
            }
            _ => {}
        }
        std::fmt::write(
            &mut bytes,
            format_args!(
                "{:?}",
                (
                    &self.soundcloud.catalog.route,
                    &self.soundcloud.items,
                    &self.soundcloud.query,
                    &self.soundcloud.submitted_query,
                    &self.soundcloud.tag,
                    &self.soundcloud.query_urn,
                )
            ),
        )
        .ok()?;
        Some(bytes.0 * 4)
    }

    /// Restores cached parent rows and selection without refetching or changing playback.
    pub(in crate::app) fn go_back_soundcloud_catalog(&mut self) -> bool {
        let Some(snapshot) = self.soundcloud.catalog.history.pop_back() else {
            return false;
        };
        self.restore_soundcloud_snapshot(snapshot);
        true
    }

    fn restore_soundcloud_snapshot(&mut self, snapshot: Snapshot) {
        self.invalidate_soundcloud_requests();
        self.soundcloud.catalog.route = snapshot.route;
        self.soundcloud.query = snapshot.query;
        self.soundcloud.submitted_query = snapshot.submitted_query;
        self.soundcloud.tag = snapshot.tag;
        self.soundcloud.items = snapshot.items;
        self.soundcloud.selected = snapshot.selected;
        self.soundcloud.next_page = snapshot.next_page;
        self.soundcloud.query_urn = snapshot.query_urn;
        self.soundcloud.page_limit = snapshot.page_limit;
        self.view.search_query.clone_from(&self.soundcloud.query);
        self.view.soundcloud_back_available = !self.soundcloud.catalog.history.is_empty();
        self.populate_soundcloud();
    }

    fn invalidate_soundcloud_requests(&mut self) {
        self.soundcloud.restored_query = None;
        self.soundcloud.generation = self.soundcloud.generation.wrapping_add(1);
        self.soundcloud.pending = None;
        self.soundcloud.page_turn = None;
        self.soundcloud.catalog.generation = self.soundcloud.catalog.generation.wrapping_add(1);
        self.soundcloud.catalog.pending = None;
        self.soundcloud.catalog.page_turn = None;
        self.finish_search_activity(SearchActivity::SoundCloud);
        self.invalidate_youtube_video_comments_popup();
    }

    fn open_soundcloud_catalog(&mut self, request: Request) {
        self.push_soundcloud_snapshot();
        self.invalidate_soundcloud_requests();
        self.soundcloud.catalog.route = Route::Loading;
        self.soundcloud.tag = None;
        self.soundcloud.items.clear();
        self.soundcloud.next_page = None;
        self.soundcloud.selected = 0;
        self.soundcloud.page_limit = Some(self.soundcloud.search_page_capacity.unwrap_or(50));
        self.soundcloud.catalog.pending = Some(Job {
            generation: self.soundcloud.catalog.generation,
            limit: self.soundcloud.page_limit.unwrap_or(50),
            append: false,
            request,
        });
        self.view.soundcloud_back_available = !self.soundcloud.catalog.history.is_empty();
        self.show_screen(Screen::SoundCloud);
        self.populate_soundcloud();
        // The ordinary controller tick starts metadata work, never a hidden prefetch.
    }

    /// Drains stale work before starting the latest catalogue intent. Search shares this lane.
    pub(super) fn poll_soundcloud_catalog(&mut self) {
        if self
            .soundcloud
            .catalog
            .worker
            .as_ref()
            .is_some_and(|worker| worker.thread.is_finished())
        {
            let worker = self
                .soundcloud
                .catalog
                .worker
                .take()
                .expect("finished catalogue worker");
            let result = worker.response.try_recv().unwrap_or_else(|_| {
                Err("Soundcloak catalogue worker stopped without a result".into())
            });
            let _ = worker.thread.join();
            self.apply_soundcloud_catalog(worker.job, result);
        }
        if self.soundcloud.catalog.worker.is_some()
            || self.soundcloud.worker.is_some()
            || self.view.screen != Screen::SoundCloud
            || self.view.quitting
            || self.diagnostic_only
        {
            return;
        }
        let Some(job) = self.soundcloud.catalog.pending.take() else {
            return;
        };
        let client = match self.soundcloak_client() {
            Ok(client) => client,
            Err(error) => {
                self.apply_soundcloud_catalog(job, Err(error));
                return;
            }
        };
        let task = job.clone();
        let (sender, response) = bounded(1);
        match thread::Builder::new()
            .name("youta-soundcloak-catalog".into())
            .spawn(move || {
                let _ = sender.send(fetch_catalog(&client, &task));
            }) {
            Ok(thread) => {
                self.soundcloud.catalog.worker = Some(Worker {
                    job,
                    response,
                    thread,
                })
            }
            Err(error) => self.apply_soundcloud_catalog(job, Err(error.to_string())),
        }
    }

    pub(super) fn soundcloud_catalog_busy(&self) -> bool {
        self.soundcloud.catalog.worker.is_some() || self.soundcloud.catalog.pending.is_some()
    }

    /// User navigation relinquishes a pending automatic one-page scroll permanently.
    pub(in crate::app) fn cancel_soundcloud_catalog_page_turn(&mut self) {
        self.soundcloud.catalog.page_turn = None;
    }

    /// A later return to the same row does not reacquire an earlier continuation intent.
    pub(super) fn update_soundcloud_catalog_selection(&mut self) {
        if self
            .soundcloud
            .catalog
            .page_turn
            .is_some_and(|(_, row)| row != self.view.selected)
        {
            self.cancel_soundcloud_catalog_page_turn();
        }
    }

    fn apply_soundcloud_catalog(&mut self, job: Job, result: Result<Page, String>) {
        if job.generation != self.soundcloud.catalog.generation {
            return;
        }
        self.finish_search_activity(SearchActivity::SoundCloud);
        match result {
            Ok(mut page) => {
                if job.append {
                    match (&self.soundcloud.catalog.route, &mut page.route) {
                        (Route::Albums { items: old, .. }, Route::Albums { items, .. }) => {
                            let mut merged = old.clone();
                            for item in items.drain(..) {
                                if !merged.iter().any(|old| old.webpage_url == item.webpage_url) {
                                    merged.push(item);
                                }
                            }
                            *items = merged;
                        }
                        (Route::Album { slots: old, .. }, Route::Album { slots, .. }) => {
                            let mut merged = old.clone();
                            merged.append(slots);
                            *slots = merged;
                        }
                        _ => {}
                    }
                    for track in page.tracks {
                        if !self
                            .soundcloud
                            .items
                            .iter()
                            .any(|old| old.webpage_url == track.webpage_url)
                        {
                            self.soundcloud.items.push(track);
                        }
                    }
                } else {
                    self.soundcloud.items = page.tracks;
                }
                self.soundcloud.items.truncate(1_000);
                match &mut page.route {
                    Route::Tracks { next, .. } if self.soundcloud.items.len() >= 1_000 => {
                        *next = None
                    }
                    Route::Albums { items, next, .. } => {
                        items.truncate(1_000);
                        if items.len() >= 1_000 {
                            *next = None;
                        }
                    }
                    Route::Album { slots, next, .. } => {
                        slots.truncate(1_000);
                        if slots.len() >= 1_000 {
                            *next = None;
                        }
                    }
                    _ => {}
                }
                self.soundcloud.catalog.route = page.route;
                if let Route::Album { slots, .. } = &self.soundcloud.catalog.route {
                    // An album can intentionally repeat a track. Preserve occurrences for
                    // autoplay exactly as displayed, unlike deduplicated artist/search pages.
                    self.soundcloud.items =
                        slots.iter().filter_map(|slot| slot.track.clone()).collect();
                }
                if let Some((generation, boundary)) = self.soundcloud.catalog.page_turn.take()
                    && generation == job.generation
                    && self.view.screen == Screen::SoundCloud
                    && self.soundcloud.selected == boundary
                {
                    let length = self.soundcloud_catalog_length();
                    self.soundcloud.selected = if self.soundcloud_catalog_has_more() {
                        length
                    } else {
                        length.saturating_sub(1)
                    };
                }
                if self.view.screen == Screen::SoundCloud {
                    self.populate_soundcloud();
                    self.refresh_selected_playlist_state();
                }
            }
            Err(error) => {
                self.soundcloud.catalog.page_turn = None;
                if self.view.screen == Screen::SoundCloud {
                    self.view.status_line =
                        format!("Soundcloak ({}): {error}", self.soundcloak_instance_label());
                }
            }
        }
    }

    fn soundcloud_catalog_length(&self) -> usize {
        match &self.soundcloud.catalog.route {
            Route::Albums { items, .. } => items.len(),
            Route::Album { slots, .. } => slots.len(),
            _ => self.soundcloud.items.len(),
        }
    }

    pub(super) fn soundcloud_catalog_has_more(&self) -> bool {
        match &self.soundcloud.catalog.route {
            Route::Tracks { next, .. } | Route::Albums { next, .. } => next.is_some(),
            Route::Album { next, .. } => next.is_some(),
            _ => false,
        }
    }

    /// Matches autoplay to the selected album occurrence, including repeated track IDs.
    pub(super) fn soundcloud_album_autoplay_index(&self, id: &MediaId) -> Option<usize> {
        if self.view.screen != Screen::SoundCloud {
            return None;
        }
        let Route::Album { slots, .. } = &self.soundcloud.catalog.route else {
            return None;
        };
        let track = slots.get(self.view.selected)?.track.as_ref()?;
        if !track.streamable || soundcloud_media_id(track) != *id {
            return None;
        }
        Some(
            slots[..self.view.selected]
                .iter()
                .filter(|slot| slot.track.as_ref().is_some_and(|track| track.streamable))
                .count(),
        )
    }

    /// Handles album folders and opaque continuations before ordinary track playback.
    pub(super) fn activate_soundcloud_catalog_selection(&mut self) -> bool {
        if let Route::Albums { items, .. } = &self.soundcloud.catalog.route
            && let Some(album) = items.get(self.view.selected)
        {
            self.open_soundcloud_catalog(Request::Album {
                url: album.webpage_url.clone(),
            });
            return true;
        }
        if self.view.selected != self.soundcloud_catalog_length()
            || !self.soundcloud_catalog_has_more()
        {
            return false;
        }
        if self.soundcloud_catalog_busy() {
            return true;
        }
        let request = match &self.soundcloud.catalog.route {
            Route::Tracks {
                artist,
                next: Some(cursor),
            } => Request::Tracks {
                artist: artist.clone(),
                cursor: cursor.clone(),
            },
            Route::Albums {
                artist,
                next: Some(cursor),
                ..
            } => Request::Albums {
                artist: artist.clone(),
                cursor: cursor.clone(),
            },
            Route::Album {
                details,
                next: Some(offset),
                ..
            } => Request::AlbumPage {
                details: details.clone(),
                offset: *offset,
            },
            _ => return false,
        };
        self.soundcloud.catalog.page_turn =
            Some((self.soundcloud.catalog.generation, self.view.selected));
        self.soundcloud.catalog.pending = Some(Job {
            generation: self.soundcloud.catalog.generation,
            limit: self.soundcloud.page_limit.unwrap_or(50),
            append: true,
            request,
        });
        self.begin_search_activity(SearchActivity::SoundCloud);
        true
    }

    pub(super) fn soundcloud_catalog_rows(&self) -> Option<Vec<RowView>> {
        match &self.soundcloud.catalog.route {
            Route::Loading => Some(Vec::new()),
            Route::Albums { items, .. } => Some(
                items
                    .iter()
                    .map(|album| RowView {
                        title: album.title.clone(),
                        subtitle: format!(
                            "{} · {} · {} tracks",
                            album.artist,
                            album.release_type.as_deref().unwrap_or("album"),
                            album
                                .track_count
                                .map_or_else(|| "?".into(), |count| count.to_string())
                        ),
                        thumbnail_url: album.artwork_url.clone(),
                        source: "SoundCloud".into(),
                        compact: true,
                        ..RowView::default()
                    })
                    .collect(),
            ),
            Route::Album { slots, .. } => Some(
                slots
                    .iter()
                    .map(|slot| {
                        slot.track.as_ref().map_or_else(
                            || RowView {
                                title: format!("Track {} · unavailable", slot.id),
                                source: "SoundCloud".into(),
                                compact: true,
                                ..RowView::default()
                            },
                            |track| self.soundcloud_track_row(track),
                        )
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Artist links use advertised canonical profiles, never display-name guessing.
    pub(super) fn soundcloud_artist_links(
        &self,
        url: &url::Url,
        label: &str,
    ) -> Vec<DetailLinkView> {
        let Ok(proxy) = self.soundcloak_client().and_then(|client| {
            client
                .profile_page_url(url)
                .map_err(|error| error.to_string())
        }) else {
            return Vec::new();
        };
        vec![
            DetailLinkView {
                prefix: "Artist: ".into(),
                label: label.into(),
                url: url.to_string(),
                ..DetailLinkView::default()
            },
            DetailLinkView {
                label: "Artist on Soundcloak".into(),
                url: proxy.to_string(),
                ..DetailLinkView::default()
            },
            DetailLinkView {
                label: "Artist tracks".into(),
                url: url.to_string(),
                internal_target: Some(DetailLinkInternalTarget::SoundCloudArtist(url.to_string())),
                ..DetailLinkView::default()
            },
            DetailLinkView {
                label: "Artist albums".into(),
                url: url.to_string(),
                internal_target: Some(DetailLinkInternalTarget::SoundCloudArtistAlbums(
                    url.to_string(),
                )),
                ..DetailLinkView::default()
            },
        ]
    }

    pub(super) fn soundcloud_catalog_detail(&self) -> Option<DetailView> {
        let (title, description, url, artwork, links) = match &self.soundcloud.catalog.route {
            Route::Tracks { artist, .. } | Route::Albums { artist, .. } => {
                if let Route::Albums { items, .. } = &self.soundcloud.catalog.route
                    && let Some(album) = items.get(self.view.selected)
                {
                    return Some(DetailView {
                        title: album.title.clone(),
                        channel_name: album.artist.clone(),
                        source: "SoundCloud album".into(),
                        description: format!(
                            "{} tracks · Enter opens this release",
                            album.track_count.map_or_else(
                                || "Unknown number of".into(),
                                |count| count.to_string()
                            )
                        ),
                        webpage_url: Some(album.webpage_url.clone()),
                        thumbnail_url: album.artwork_url.clone(),
                        links: self.soundcloud_artist_links(&artist.webpage_url, &artist.name),
                        ..DetailView::default()
                    });
                }
                (
                    artist.name.clone(),
                    "Choose Tracks or Albums; Back returns to the previous list.".into(),
                    artist.webpage_url.clone(),
                    None,
                    self.soundcloud_artist_links(&artist.webpage_url, &artist.name),
                )
            }
            Route::Album { details, slots, .. } => {
                let album = &details.album;
                let description = slots
                    .get(self.view.selected)
                    .filter(|slot| slot.track.is_none())
                    .map_or_else(
                        || {
                            "Ordered album tracks; unavailable public slots retain their position."
                                .into()
                        },
                        |slot| {
                            format!(
                                "Track {} is unavailable; no playable URL was provided.",
                                slot.id
                            )
                        },
                    );
                (
                    album.title.clone(),
                    description,
                    album.webpage_url.clone(),
                    album.artwork_url.clone(),
                    album.artist_url.as_ref().map_or_else(Vec::new, |url| {
                        self.soundcloud_artist_links(url, &album.artist)
                    }),
                )
            }
            _ => return None,
        };
        Some(DetailView {
            title,
            description,
            source: "SoundCloud".into(),
            webpage_url: Some(url),
            thumbnail_url: artwork,
            links,
            ..DetailView::default()
        })
    }

}

/// Allocation-free metadata counter which fails immediately before exceeding its cap.
#[cfg(feature = "soundcloud")]
struct HistoryBytes(usize);

#[cfg(feature = "soundcloud")]
impl HistoryBytes {
    /// Includes inline storage even for large vectors of unavailable album slots.
    fn rows<T>(&mut self, count: usize) -> Option<()> {
        self.add(count.checked_mul(std::mem::size_of::<T>())?)
    }

    /// Reserves the fourfold safety margin before accepting any additional bytes.
    fn add(&mut self, bytes: usize) -> Option<()> {
        let total = self.0.checked_add(bytes)?;
        if total > MAX_HISTORY_BYTES / 4 {
            return None;
        }
        self.0 = total;
        Some(())
    }
}

#[cfg(feature = "soundcloud")]
impl std::fmt::Write for HistoryBytes {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.add(text.len()).ok_or(std::fmt::Error)
    }
}

/// Skips at most four empty raw pages per explicit request, retaining a remaining cursor.
#[cfg(feature = "soundcloud")]
fn fetch_catalog(client: &SoundcloakClient, job: &Job) -> Result<Page, String> {
    let result = (|| -> Result<Page, crate::providers::ProviderError> {
        let (artist, albums, mut cursor) = match &job.request {
            Request::Artist { url, albums } => (client.resolve_artist(url)?, *albums, None),
            Request::Tracks { artist, cursor } => (artist.clone(), false, Some(cursor.clone())),
            Request::Albums { artist, cursor } => (artist.clone(), true, Some(cursor.clone())),
            Request::Album { url } => {
                let details = Arc::new(client.resolve_album(url)?);
                return fetch_album_page(client, details, 0, job.limit);
            }
            Request::AlbumPage { details, offset } => {
                return fetch_album_page(client, details.clone(), *offset, job.limit);
            }
        };
        for attempt in 0..4 {
            if albums {
                let page = client.artist_albums(&artist, job.limit, cursor.as_ref())?;
                if !page.items.is_empty() || page.next_cursor.is_none() || attempt == 3 {
                    return Ok(Page {
                        route: Route::Albums {
                            artist,
                            items: page.items,
                            next: page.next_cursor,
                        },
                        tracks: Vec::new(),
                    });
                }
                cursor = page.next_cursor;
            } else {
                let page = client.artist_tracks(&artist, job.limit, cursor.as_ref())?;
                if !page.items.is_empty() || page.next_cursor.is_none() || attempt == 3 {
                    return Ok(Page {
                        route: Route::Tracks {
                            artist,
                            next: page.next_cursor,
                        },
                        tracks: page.items,
                    });
                }
                cursor = page.next_cursor;
            }
        }
        unreachable!("the last bounded page always returns")
    })();
    result.map_err(|error| error.to_string())
}

#[cfg(feature = "soundcloud")]
fn fetch_album_page(
    client: &SoundcloakClient,
    details: Arc<SoundcloakAlbumDetails>,
    offset: usize,
    limit: usize,
) -> Result<Page, crate::providers::ProviderError> {
    let page = client.album_tracks(&details, offset, limit)?;
    let tracks = page
        .items
        .iter()
        .filter_map(|slot| slot.track.clone())
        .collect();
    Ok(Page {
        route: Route::Album {
            details,
            slots: page.items,
            next: page.next_offset,
        },
        tracks,
    })
}

#[cfg(all(test, feature = "soundcloud"))]
#[path = "artist_tests.rs"]
mod integration_tests;

#[cfg(all(test, feature = "soundcloud"))]
mod tests {
    use super::*;

    #[test]
    fn soundcloud_artist_action_keeps_a_cached_back_destination_and_loads_tracks() {
        let mut app = super::super::tests::controller();
        super::super::tests::complete(&mut app, 0, vec![super::super::tests::track("one")], None);
        app.view.external_opener_available = false;
        app.dispatch(UiAction::OpenSoundCloudArtist(
            "https://soundcloud.com/fixture-artist".into(),
        ));
        assert!(
            app.view.soundcloud_back_available,
            "artist navigation must retain the search without an external browser"
        );
        assert_eq!(app.view.search_activity, Some(SearchActivity::SoundCloud));
    }
}
