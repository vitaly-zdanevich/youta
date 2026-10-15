//! Pod-scoped Funkwhale browsing and notified, bounded HTTP work.
//!
//! Saved queues contain canonical track pages, never resolved audio locators.
//! Opening setup does not select a pod or fetch a directory implicitly.

#[cfg(feature = "funkwhale")]
mod license;
#[cfg(feature = "funkwhale")]
mod retry;
#[cfg(feature = "funkwhale")]
mod uploader;

#[cfg(feature = "funkwhale")]
use super::worker_notifications::reap_published_worker;
use super::*;

#[cfg(feature = "funkwhale")]
use crate::view::{FunkwhalePodPickerView, FunkwhalePodView, FunkwhaleSetupPopupView};

#[cfg(feature = "funkwhale")]
use crate::providers::funkwhale::{
    FunkwhaleAlbum, FunkwhaleAlbumPage, FunkwhalePod, FunkwhaleProvider, FunkwhaleUploadMetadata,
    FunkwhaleUploader, discover_public_pods,
};
#[cfg(feature = "funkwhale")]
use crate::providers::{Provider, SearchRequest, SearchTarget};

/// Retains independent navigation across feature-trimmed builds.
#[derive(Default)]
pub(super) struct FunkwhaleState {
    pub(super) query: String,
    pub(super) selected: usize,
    #[cfg(feature = "funkwhale")]
    items: Vec<VideoSummary>,
    #[cfg(feature = "funkwhale")]
    albums: Vec<FunkwhaleAlbum>,
    #[cfg(feature = "funkwhale")]
    standalone: bool,
    /// Unknown keeps the entry accessible; only a successful absence probe hides it.
    #[cfg(feature = "funkwhale")]
    standalone_presence: Option<bool>,
    #[cfg(feature = "funkwhale")]
    details: HashMap<String, VideoDetails>,
    #[cfg(feature = "funkwhale")]
    upload_metadata: HashMap<String, FunkwhaleUploadMetadata>,
    #[cfg(feature = "funkwhale")]
    upload_metadata_checked: HashSet<String>,
    #[cfg(feature = "funkwhale")]
    failed_details: HashSet<String>,
    #[cfg(feature = "funkwhale")]
    jobs: VecDeque<Job>,
    #[cfg(feature = "funkwhale")]
    worker: Option<Worker>,
    #[cfg(feature = "funkwhale")]
    generation: u64,
    #[cfg(feature = "funkwhale")]
    playback_generation: u64,
    #[cfg(feature = "funkwhale")]
    playback_retry: Option<retry::FunkwhalePlaybackRetry>,
    #[cfg(feature = "funkwhale")]
    admitting: Option<MediaId>,
    #[cfg(feature = "funkwhale")]
    next_page: Option<u32>,
    #[cfg(feature = "funkwhale")]
    capacity: Option<crate::view::SearchPageCapacities>,
    #[cfg(feature = "funkwhale")]
    page_size: usize,
    #[cfg(feature = "funkwhale")]
    page_turn: Option<usize>,
    #[cfg(feature = "funkwhale")]
    restore: bool,
    #[cfg(feature = "funkwhale")]
    restore_selected: Option<usize>,
    #[cfg(feature = "funkwhale")]
    setup_prompted: bool,
    #[cfg(feature = "funkwhale")]
    pods: Option<Vec<FunkwhalePod>>,
    #[cfg(feature = "funkwhale")]
    clients: VecDeque<FunkwhaleProvider>,
    #[cfg(feature = "funkwhale")]
    artist: Option<(String, String)>,
    #[cfg(feature = "funkwhale")]
    album: Option<(String, String)>,
    #[cfg(feature = "funkwhale")]
    tag: Option<(String, url::Url)>,
    #[cfg(feature = "funkwhale")]
    uploader: Option<(FunkwhaleUploader, url::Url)>,
    #[cfg(feature = "funkwhale")]
    recent_attempted: bool,
    #[cfg(feature = "funkwhale")]
    artist_back: VecDeque<ArtistReturn>,
}

impl FunkwhaleState {
    /// Hydrates only query and selection; every network operation remains deferred.
    pub(super) fn restored(saved: &SessionState) -> Self {
        Self {
            query: saved.funkwhale_search_text.clone(),
            selected: saved.funkwhale_selected_row.unwrap_or_default(),
            #[cfg(feature = "funkwhale")]
            restore: !saved.funkwhale_search_text.trim().is_empty(),
            #[cfg(feature = "funkwhale")]
            restore_selected: saved.funkwhale_selected_row,
            ..Self::default()
        }
    }
}

/// One bounded source operation runs while at most four latest intents wait.
#[cfg(feature = "funkwhale")]
#[derive(Clone)]
enum Job {
    Search {
        generation: u64,
        pod: url::Url,
        query: String,
        page: u32,
        size: usize,
        artist: Option<String>,
        album: Option<String>,
        tag: Option<String>,
        standalone: bool,
        uploader: Option<FunkwhaleUploader>,
    },
    Details {
        generation: u64,
        id: String,
    },
    Playback {
        generation: u64,
        item: Box<QueueItem>,
        queued: bool,
        origin: Option<AutoplayOrigin>,
    },
    Pods,
}

/// Owned results are published before the worker is reaped, waking both frontends.
#[cfg(feature = "funkwhale")]
enum Reply {
    Search(crate::providers::SearchPage),
    Albums(FunkwhaleAlbumPage, Option<Result<bool, String>>),
    DetailsWithUpload(VideoDetails, Option<FunkwhaleUploadMetadata>),
    Playback(VideoDetails, url::Url),
    Pods(Vec<FunkwhalePod>),
}

#[cfg(feature = "funkwhale")]
struct Worker {
    job: Job,
    response: Receiver<Result<Reply, String>>,
    thread: JoinHandle<()>,
}

/// Typed source rows retained by the shared now-playing Back navigation.
#[cfg(feature = "funkwhale")]
#[derive(Clone)]
pub(super) struct FunkwhaleNavigationSnapshot {
    items: Vec<VideoSummary>,
    albums: Vec<FunkwhaleAlbum>,
    standalone: bool,
    standalone_presence: Option<bool>,
    next_page: Option<u32>,
    page_size: usize,
    artist: Option<(String, String)>,
    album: Option<(String, String)>,
    tag: Option<(String, url::Url)>,
    uploader: Option<(FunkwhaleUploader, url::Url)>,
}

/// One explicit artist navigation's previous pod catalogue, query and cursor.
#[cfg(feature = "funkwhale")]
struct ArtistReturn {
    page: FunkwhaleNavigationSnapshot,
    query: String,
    selected: usize,
}

/// Playback refreshes ordinary track facts without repeating optional upload lookups.
#[cfg(feature = "funkwhale")]
enum UploadMetadataUpdate {
    Unchanged,
    Replace(Option<FunkwhaleUploadMetadata>),
}

impl AppController {
    /// Distinguishes non-playable album rows from every track-based Funkwhale route.
    #[cfg(feature = "funkwhale")]
    fn funkwhale_artist_directory(&self) -> bool {
        self.funkwhale.artist.is_some() && !self.funkwhale.standalone
    }

    /// Keeps uncertain artist catalogues reachable without showing proven-empty folders.
    #[cfg(feature = "funkwhale")]
    fn funkwhale_standalone_entry_visible(&self) -> bool {
        self.funkwhale_artist_directory() && self.funkwhale.standalone_presence != Some(false)
    }

    /// Opens one explicitly requested artist-track page, excluding album-bound tracks.
    #[cfg(feature = "funkwhale")]
    fn open_funkwhale_standalone_tracks(&mut self) {
        if !self.funkwhale_artist_directory() {
            return;
        }
        let page = FunkwhaleNavigationSnapshot {
            items: self.funkwhale.items.clone(),
            albums: self.funkwhale.albums.clone(),
            standalone: false,
            standalone_presence: self.funkwhale.standalone_presence,
            next_page: self.funkwhale.next_page,
            page_size: self.funkwhale.page_size,
            artist: self.funkwhale.artist.clone(),
            album: self.funkwhale.album.clone(),
            tag: self.funkwhale.tag.clone(),
            uploader: self.funkwhale.uploader.clone(),
        };
        if self.funkwhale.artist_back.len() == 16 {
            self.funkwhale.artist_back.pop_front();
        }
        self.funkwhale.artist_back.push_back(ArtistReturn {
            page,
            query: self.funkwhale.query.clone(),
            selected: self.view.selected,
        });
        self.funkwhale.standalone = true;
        self.start_funkwhale_catalogue(String::new());
    }

    /// Resolves only the selected retained track's exact artist on its original pod.
    #[cfg(feature = "funkwhale")]
    fn selected_funkwhale_artist(&self) -> Option<(String, String)> {
        if self.view.screen != Screen::Funkwhale {
            return None;
        }
        let track = self.funkwhale.items.get(self.view.selected)?;
        let pod = FunkwhaleProvider::instance_url_from_track_id(&track.video_id).ok()?;
        let artist_pod = FunkwhaleProvider::instance_url_from_artist_id(&track.channel_id).ok()?;
        if artist_pod != pod {
            return None;
        }
        Some((track.channel_id.clone(), track.channel_name.clone()))
    }

    /// Reports whether the selected source row has a proven pod-local artist identity.
    #[cfg(feature = "funkwhale")]
    pub(super) fn can_open_funkwhale_artist(&self) -> bool {
        self.selected_funkwhale_artist().is_some()
    }

    /// Opens an artist's album directory, keeping playable tracks one level below it.
    #[cfg(feature = "funkwhale")]
    pub(super) fn open_funkwhale_artist(&mut self) {
        let Some(artist) = self.selected_funkwhale_artist() else {
            return;
        };
        if self.funkwhale.artist.as_ref() == Some(&artist) && !self.funkwhale.standalone {
            return;
        }
        let page = FunkwhaleNavigationSnapshot {
            items: self.funkwhale.items.clone(),
            albums: self.funkwhale.albums.clone(),
            standalone: self.funkwhale.standalone,
            standalone_presence: self.funkwhale.standalone_presence,
            next_page: self.funkwhale.next_page,
            page_size: self.funkwhale.page_size,
            artist: self.funkwhale.artist.clone(),
            album: self.funkwhale.album.clone(),
            tag: self.funkwhale.tag.clone(),
            uploader: self.funkwhale.uploader.clone(),
        };
        if self.funkwhale.artist_back.len() == 16 {
            self.funkwhale.artist_back.pop_front();
        }
        self.funkwhale.artist_back.push_back(ArtistReturn {
            page,
            query: self.funkwhale.query.clone(),
            selected: self.view.selected,
        });
        self.funkwhale.artist = Some(artist);
        self.funkwhale.standalone = false;
        self.funkwhale.standalone_presence = None;
        self.funkwhale.album = None;
        self.funkwhale.tag = None;
        self.funkwhale.uploader = None;
        self.start_funkwhale_catalogue(String::new());
    }

    /// Opens only the selected track's advertised original-pod album with reversible navigation.
    #[cfg(feature = "funkwhale")]
    pub(super) fn open_funkwhale_album(&mut self, url: String) {
        if self.view.screen != Screen::Funkwhale {
            return;
        }
        let album = if self.funkwhale_artist_directory() {
            self.funkwhale
                .albums
                .get(self.view.selected)
                .map(|album| (album.url.to_string(), album.title.clone()))
        } else {
            self.funkwhale
                .items
                .get(self.view.selected)
                .and_then(|track| track_album(track, self.funkwhale.details.get(&track.video_id)))
        };
        let Some(album) = album else {
            return;
        };
        if album.0 != url || self.funkwhale.album.as_ref() == Some(&album) {
            return;
        }
        let page = FunkwhaleNavigationSnapshot {
            items: self.funkwhale.items.clone(),
            albums: self.funkwhale.albums.clone(),
            standalone: self.funkwhale.standalone,
            standalone_presence: self.funkwhale.standalone_presence,
            next_page: self.funkwhale.next_page,
            page_size: self.funkwhale.page_size,
            artist: self.funkwhale.artist.clone(),
            album: self.funkwhale.album.clone(),
            tag: self.funkwhale.tag.clone(),
            uploader: self.funkwhale.uploader.clone(),
        };
        if self.funkwhale.artist_back.len() == 16 {
            self.funkwhale.artist_back.pop_front();
        }
        self.funkwhale.artist_back.push_back(ArtistReturn {
            page,
            query: self.funkwhale.query.clone(),
            selected: self.view.selected,
        });
        self.funkwhale.artist = None;
        self.funkwhale.standalone = false;
        self.funkwhale.standalone_presence = None;
        self.funkwhale.album = Some(album);
        self.funkwhale.tag = None;
        self.funkwhale.uploader = None;
        self.start_funkwhale_catalogue(String::new());
    }

    /// Opens a clicked tag on the selected track's pod with reversible navigation.
    #[cfg(feature = "funkwhale")]
    pub(super) fn search_funkwhale_tag(&mut self, tag: String) {
        if self.view.screen != Screen::Funkwhale {
            return;
        }
        let Some(track) = self.funkwhale.items.get(self.view.selected) else {
            return;
        };
        if !self
            .funkwhale
            .details
            .get(&track.video_id)
            .is_some_and(|details| details.keywords.iter().any(|value| value == &tag))
        {
            return;
        }
        let Ok(pod) = FunkwhaleProvider::instance_url_from_track_id(&track.video_id) else {
            return;
        };
        if tag.trim().is_empty() || tag.len() > 1024 || tag.chars().any(char::is_control) {
            return;
        }
        let page = FunkwhaleNavigationSnapshot {
            items: self.funkwhale.items.clone(),
            albums: self.funkwhale.albums.clone(),
            standalone: self.funkwhale.standalone,
            standalone_presence: self.funkwhale.standalone_presence,
            next_page: self.funkwhale.next_page,
            page_size: self.funkwhale.page_size,
            artist: self.funkwhale.artist.clone(),
            album: self.funkwhale.album.clone(),
            tag: self.funkwhale.tag.clone(),
            uploader: self.funkwhale.uploader.clone(),
        };
        if self.funkwhale.artist_back.len() == 16 {
            self.funkwhale.artist_back.pop_front();
        }
        self.funkwhale.artist_back.push_back(ArtistReturn {
            page,
            query: self.funkwhale.query.clone(),
            selected: self.view.selected,
        });
        self.funkwhale.artist = None;
        self.funkwhale.standalone = false;
        self.funkwhale.standalone_presence = None;
        self.funkwhale.album = None;
        self.funkwhale.tag = Some((tag.clone(), pod));
        self.funkwhale.uploader = None;
        self.start_funkwhale_catalogue(tag);
    }

    /// Restores one artist's previous source catalogue while rejecting old completions.
    #[cfg(feature = "funkwhale")]
    pub(super) fn back_funkwhale_artist(&mut self) -> bool {
        if self.view.screen != Screen::Funkwhale {
            return false;
        }
        let Some(mut previous) = self.funkwhale.artist_back.pop_back() else {
            return false;
        };
        self.swap_funkwhale_navigation(&mut previous.page);
        self.funkwhale.query = previous.query;
        self.view.search_query.clone_from(&self.funkwhale.query);
        self.funkwhale.selected = previous.selected;
        self.populate_funkwhale();
        true
    }

    /// Opens explicit pod configuration without network access or an implicit default.
    pub(super) fn open_funkwhale_setup(&mut self) {
        #[cfg(feature = "funkwhale")]
        {
            self.funkwhale.setup_prompted = true;
            self.view.funkwhale_setup_popup = Some(FunkwhaleSetupPopupView {
                url: self
                    .config
                    .providers
                    .funkwhale_instance_url
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string),
                config_file_path: self.config.config_file().display().to_string(),
                ..FunkwhaleSetupPopupView::default()
            });
        }
        #[cfg(not(feature = "funkwhale"))]
        {
            self.view.status_line = "This build omits the `funkwhale` feature".to_owned();
        }
    }

    /// Applies setup actions without allowing underlying playback shortcuts through the modal.
    pub(super) fn dispatch_funkwhale_setup(&mut self, action: UiAction) {
        if matches!(action, UiAction::OpenFunkwhaleSetup) {
            self.open_funkwhale_setup();
            return;
        }
        #[cfg(feature = "funkwhale")]
        {
            if matches!(action, UiAction::SaveFunkwhaleSetup) {
                self.save_funkwhale_setup();
                return;
            }
            if matches!(action, UiAction::OpenFunkwhalePodPicker) {
                self.open_funkwhale_pods();
                return;
            }
            let Some(popup) = self.view.funkwhale_setup_popup.as_mut() else {
                return;
            };
            match action {
                UiAction::CloseFunkwhaleSetup => {
                    self.view.funkwhale_setup_popup = None;
                }
                UiAction::SetFunkwhalePodUrl(value) => {
                    if value.len() <= 2048 && !value.chars().any(char::is_control) {
                        popup.url = value;
                        popup.error = None;
                    }
                }
                UiAction::EditFunkwhalePodUrl(character)
                    if !character.is_control()
                        && popup.url.len() + character.len_utf8() <= 2048 =>
                {
                    popup.url.push(character);
                    popup.error = None;
                }
                UiAction::BackspaceFunkwhalePodUrl => {
                    if let Some((index, _)) = popup.url.grapheme_indices(true).next_back() {
                        popup.url.truncate(index);
                    }
                    popup.error = None;
                }
                UiAction::ClearFunkwhalePodUrl => {
                    popup.url.clear();
                    popup.error = None;
                }
                UiAction::CloseFunkwhalePodPicker => {
                    popup.picker = None;
                }
                UiAction::MoveFunkwhalePod(delta) => {
                    if let Some(picker) = popup.picker.as_mut() {
                        picker.selected = picker
                            .selected
                            .saturating_add_signed(isize::try_from(delta).unwrap_or_default())
                            .min(picker.pods.len().saturating_sub(1));
                    }
                }
                UiAction::SelectFunkwhalePod(index) => {
                    if let Some(pod) = popup
                        .picker
                        .as_ref()
                        .and_then(|picker| picker.pods.get(index))
                    {
                        popup.url = pod.url.clone();
                        popup.picker = None;
                        popup.error = None;
                    }
                }
                _ => {}
            }
        }
        #[cfg(not(feature = "funkwhale"))]
        {
            let _ = action;
        }
    }

    /// Saves a validated address before invalidating results from the previous pod.
    #[cfg(feature = "funkwhale")]
    fn save_funkwhale_setup(&mut self) {
        let Some(popup) = self.view.funkwhale_setup_popup.as_ref() else {
            return;
        };
        let draft = popup.url.trim();
        let pod = if draft.is_empty() {
            Ok(None)
        } else {
            let address = if draft.contains("://") {
                draft.to_owned()
            } else {
                format!("https://{draft}")
            };
            url::Url::parse(&address)
                .map_err(|_| "Enter a complete HTTP(S) Funkwhale pod address".to_owned())
                .and_then(|url| {
                    FunkwhaleProvider::new(url)
                        .map(|provider| Some(provider.base_url().clone()))
                        .map_err(|_| {
                            "Enter a credential-free HTTP(S) pod address without query parameters"
                                .to_owned()
                        })
                })
        };
        let result = pod.and_then(|pod| {
            self.config
                .save_funkwhale_pod(pod)
                .map_err(|error| error.to_string())
        });
        if let Err(error) = result {
            if let Some(popup) = self.view.funkwhale_setup_popup.as_mut() {
                popup.error = Some(error);
            }
            return;
        }
        self.funkwhale.generation = self.funkwhale.generation.wrapping_add(1);
        self.cancel_funkwhale_playback();
        self.funkwhale.items.clear();
        self.funkwhale.albums.clear();
        self.funkwhale.standalone = false;
        self.funkwhale.standalone_presence = None;
        self.funkwhale.details.clear();
        self.funkwhale.upload_metadata.clear();
        self.funkwhale.upload_metadata_checked.clear();
        self.funkwhale.failed_details.clear();
        self.funkwhale.jobs.clear();
        self.funkwhale.next_page = None;
        self.funkwhale.artist = None;
        self.funkwhale.album = None;
        self.funkwhale.tag = None;
        self.funkwhale.uploader = None;
        self.funkwhale.artist_back.clear();
        self.funkwhale.recent_attempted = false;
        self.funkwhale.selected = 0;
        self.funkwhale.page_turn = None;
        self.view.funkwhale_setup_popup = None;
        self.finish_search_activity(SearchActivity::Funkwhale);
        if self.view.screen == Screen::Funkwhale {
            self.populate_funkwhale();
            if self.config.providers.funkwhale_instance_url.is_some()
                && !self.funkwhale.query.trim().is_empty()
            {
                self.submit_funkwhale_search(self.funkwhale.query.clone());
            }
        }
    }

    /// Loads only the official directory, caching its public entries until exit.
    #[cfg(feature = "funkwhale")]
    fn open_funkwhale_pods(&mut self) {
        let Some(popup) = self.view.funkwhale_setup_popup.as_mut() else {
            return;
        };
        let pods = self
            .funkwhale
            .pods
            .as_ref()
            .map(|pods| pods.iter().map(pod_view).collect());
        popup.picker = Some(FunkwhalePodPickerView {
            loading: pods.is_none(),
            pods: pods.unwrap_or_default(),
            ..FunkwhalePodPickerView::default()
        });
        if self.funkwhale.pods.is_none() {
            self.queue_funkwhale_job(Job::Pods);
        }
    }

    /// Reports a future page capacity without changing an existing page stride.
    #[cfg(feature = "funkwhale")]
    pub(super) fn update_funkwhale_search_page_capacity(&mut self, rows: usize) {
        self.update_funkwhale_search_page_capacities(crate::view::SearchPageCapacities::fixed(
            rows,
        ));
    }

    /// Retains both heading layouts until this source submits its own first-page query.
    #[cfg(feature = "funkwhale")]
    pub(super) fn update_funkwhale_search_page_capacities(
        &mut self,
        capacities: crate::view::SearchPageCapacities,
    ) {
        self.funkwhale.capacity = Some(capacities.clamped(1, 100));
    }

    /// Starts a query only on the pod explicitly chosen by the user.
    pub(super) fn submit_funkwhale_search(&mut self, query: String) {
        #[cfg(feature = "funkwhale")]
        {
            self.funkwhale.artist = None;
            self.funkwhale.standalone = false;
            self.funkwhale.standalone_presence = None;
            self.funkwhale.album = None;
            self.funkwhale.tag = None;
            self.funkwhale.uploader = None;
            self.funkwhale.artist_back.clear();
        }
        self.start_funkwhale_catalogue(query);
    }

    /// Keeps artist pagination on its original pod even after a different pod is configured.
    #[cfg(feature = "funkwhale")]
    pub(super) fn funkwhale_catalogue_pod(&self) -> Option<url::Url> {
        self.funkwhale
            .artist
            .as_ref()
            .and_then(|(identity, _)| FunkwhaleProvider::instance_url_from_artist_id(identity).ok())
            .or_else(|| {
                self.funkwhale.album.as_ref().and_then(|(identity, _)| {
                    FunkwhaleProvider::instance_url_from_album_id(identity).ok()
                })
            })
            .or_else(|| self.funkwhale.tag.as_ref().map(|(_, pod)| pod.clone()))
            .or_else(|| self.funkwhale.uploader.as_ref().map(|(_, pod)| pod.clone()))
            .or_else(|| self.config.providers.funkwhale_instance_url.clone())
    }

    /// Starts one explicit query, recent catalogue or exact artist page on its owning pod.
    fn start_funkwhale_catalogue(&mut self, query: String) {
        self.funkwhale.query = query;
        if self.view.screen == Screen::Funkwhale {
            self.view.search_query.clone_from(&self.funkwhale.query);
        }
        #[cfg(feature = "funkwhale")]
        {
            self.funkwhale.restore = false;
            self.funkwhale.restore_selected = None;
            self.funkwhale.recent_attempted = true;
            let pod = self.funkwhale_catalogue_pod();
            let Some(pod) = pod else {
                self.open_funkwhale_setup();
                return;
            };
            self.funkwhale.generation = self.funkwhale.generation.wrapping_add(1);
            self.funkwhale.items.clear();
            self.funkwhale.albums.clear();
            self.funkwhale.next_page = None;
            self.funkwhale.page_turn = None;
            self.funkwhale.selected = 0;
            self.funkwhale.failed_details.clear();
            self.funkwhale
                .jobs
                .retain(|job| matches!(job, Job::Pods | Job::Playback { .. }));
            self.funkwhale.page_size = if self.funkwhale.query.trim().is_empty()
                && self.funkwhale.artist.is_none()
                && self.funkwhale.album.is_none()
                && self.funkwhale.tag.is_none()
                && self.funkwhale.uploader.is_none()
            {
                20
            } else {
                self.funkwhale
                    .capacity
                    .map_or(50, |capacity| capacity.for_query(&self.funkwhale.query))
            };
            self.populate_funkwhale();
            self.begin_search_activity(SearchActivity::Funkwhale);
            self.queue_funkwhale_job(Job::Search {
                generation: self.funkwhale.generation,
                pod,
                query: self.funkwhale.query.clone(),
                page: 1,
                size: self.funkwhale.page_size,
                artist: self
                    .funkwhale
                    .artist
                    .as_ref()
                    .map(|(identity, _)| identity.clone()),
                album: self
                    .funkwhale
                    .album
                    .as_ref()
                    .map(|(identity, _)| identity.clone()),
                tag: self.funkwhale.tag.as_ref().map(|(tag, _)| tag.clone()),
                standalone: self.funkwhale.standalone,
                uploader: self
                    .funkwhale
                    .uploader
                    .as_ref()
                    .map(|(actor, _)| actor.clone()),
            });
        }
    }

    /// Projects one compact track per terminal row and an explicit continuation only when advertised.
    pub(super) fn populate_funkwhale(&mut self) {
        #[cfg(feature = "funkwhale")]
        {
            self.view.funkwhale_back_available = !self.funkwhale.artist_back.is_empty();
            self.view.rows = if self.funkwhale_artist_directory() {
                let mut rows = self
                    .funkwhale
                    .albums
                    .iter()
                    .map(|album| RowView {
                        title: album.title.clone(),
                        subtitle: [
                            album.tracks_count.map(|count| format!("{count} tracks")),
                            album.release_date.clone(),
                        ]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · "),
                        source: "Funkwhale".to_owned(),
                        thumbnail_url: album
                            .thumbnails
                            .first()
                            .map(|thumbnail| thumbnail.url.clone()),
                        compact: true,
                        ..RowView::default()
                    })
                    .collect::<Vec<_>>();
                if self.funkwhale_standalone_entry_visible() {
                    rows.push(RowView {
                        title: "Standalone tracks".to_owned(),
                        compact: true,
                        ..RowView::default()
                    });
                }
                rows
            } else {
                self.funkwhale
                    .items
                    .iter()
                    .map(|track| RowView {
                        media_id: Some(MediaId::new(SourceKind::Funkwhale, &track.video_id)),
                        title: track.title.clone(),
                        subtitle: if self.funkwhale.artist.is_some() {
                            track
                                .description
                                .lines()
                                .find_map(|line| line.strip_prefix("Album: "))
                                .map_or_else(
                                    || track.channel_name.clone(),
                                    |album| format!("{} · {album}", track.channel_name),
                                )
                        } else {
                            track.channel_name.clone()
                        },
                        source: "Funkwhale".to_owned(),
                        thumbnail_url: track
                            .thumbnails
                            .first()
                            .map(|thumbnail| thumbnail.url.clone()),
                        compact: true,
                        ..RowView::default()
                    })
                    .collect()
            };
            hydrate_row_playback_progress(&self.store, &mut self.view.rows);
            if self.funkwhale.next_page.is_some() {
                self.view.rows.push(RowView {
                    title: if self.funkwhale_artist_directory() {
                        "Load more albums..."
                    } else {
                        "Load more tracks..."
                    }
                    .to_owned(),
                    compact: true,
                    ..RowView::default()
                });
            }
            self.view.selected = self
                .funkwhale
                .selected
                .min(self.view.rows.len().saturating_sub(1));
            self.update_funkwhale_detail();
            self.view.status_line =
                if let Some(pod) = self.config.providers.funkwhale_instance_url.as_ref() {
                    let catalogue = self
                        .funkwhale
                        .artist
                        .as_ref()
                        .map(|(_, name)| format!("Artist: {name} - "))
                        .unwrap_or_default();
                    format!(
                        "Funkwhale - {} - {}{} {}",
                        pod,
                        catalogue,
                        if self.funkwhale_artist_directory() {
                            self.funkwhale.albums.len()
                        } else {
                            self.funkwhale.items.len()
                        },
                        if self.funkwhale_artist_directory() {
                            "albums"
                        } else {
                            "tracks"
                        },
                    )
                } else {
                    "Funkwhale needs a pod address; F4 to configure".to_owned()
                };
            if self.config.providers.funkwhale_instance_url.is_none()
                && !self.funkwhale.setup_prompted
            {
                self.open_funkwhale_setup();
            }
        }
        #[cfg(not(feature = "funkwhale"))]
        {
            self.view.funkwhale_back_available = false;
            self.view.rows.clear();
            self.view.details = None;
            self.view.status_line = "This build omits the `funkwhale` feature".to_owned();
        }
    }

    /// Keeps preliminary metadata immediately available while fetching only the selected track lazily.
    pub(super) fn update_funkwhale_detail(&mut self) {
        self.funkwhale.selected = self.view.selected;
        #[cfg(feature = "funkwhale")]
        {
            if self
                .funkwhale
                .page_turn
                .is_some_and(|row| row != self.view.selected)
            {
                self.funkwhale.page_turn = None;
            }
            if self.funkwhale_artist_directory() {
                self.view.details = self.funkwhale.albums.get(self.view.selected).map(|album| {
                    let artist = self.funkwhale.artist.as_ref().expect("artist directory");
                    DetailView {
                        title: album.title.clone(),
                        source: "Funkwhale".to_owned(),
                        channel_name: artist.1.clone(),
                        channel_id: artist.0.clone(),
                        channel_webpage_url: url::Url::parse(&artist.0).ok(),
                        webpage_url: Some(album.url.clone()),
                        description: [
                            album.tracks_count.map(|count| format!("Tracks: {count}")),
                            (!album.description.is_empty()).then(|| album.description.clone()),
                        ]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                        published: album.release_date.clone().unwrap_or_default(),
                        thumbnail_url: album
                            .thumbnails
                            .first()
                            .map(|thumbnail| thumbnail.url.clone()),
                        expanded_thumbnail_url: album
                            .thumbnails
                            .first()
                            .map(|thumbnail| thumbnail.url.clone()),
                        thumbnail_fallback_urls: artwork_candidates(&album.thumbnails),
                        links: vec![DetailLinkView {
                            label: album.title.clone(),
                            url: album.url.to_string(),
                            internal_target: Some(DetailLinkInternalTarget::FunkwhaleAlbum(
                                album.url.to_string(),
                            )),
                            ..DetailLinkView::default()
                        }],
                        ..DetailView::default()
                    }
                });
                return;
            }
            let Some(track) = self.funkwhale.items.get(self.view.selected).cloned() else {
                self.view.details = None;
                return;
            };
            let details = self.funkwhale.details.get(&track.video_id);
            let mut next = detail_view(&track, details);
            self.project_funkwhale_uploader(&track, &mut next);
            preserve_thumbnail_expansion(self.view.details.as_ref(), &mut next);
            self.view.details = Some(next);
            if (details.is_none()
                || !self
                    .funkwhale
                    .upload_metadata_checked
                    .contains(&track.video_id))
                && !self.funkwhale.failed_details.contains(&track.video_id)
            {
                self.queue_funkwhale_job(Job::Details {
                    generation: self.funkwhale.generation,
                    id: track.video_id,
                });
            }
            #[cfg(feature = "acoustid")]
            self.apply_funkwhale_fingerprint_details();
        }
    }

    /// Plays a real track or turns exactly one viewport-sized continuation page.
    pub(super) fn activate_funkwhale_selection(&mut self) {
        #[cfg(feature = "funkwhale")]
        {
            let continuation = if self.funkwhale_artist_directory() {
                if let Some(album) = self.funkwhale.albums.get(self.view.selected) {
                    self.open_funkwhale_album(album.url.to_string());
                    return;
                }
                let standalone = self.funkwhale_standalone_entry_visible();
                if standalone && self.view.selected == self.funkwhale.albums.len() {
                    self.open_funkwhale_standalone_tracks();
                    return;
                }
                self.funkwhale.albums.len() + usize::from(standalone)
            } else {
                self.funkwhale.items.len()
            };
            if self.view.selected == continuation {
                if let (Some(page), Some(pod)) =
                    (self.funkwhale.next_page, self.funkwhale_catalogue_pod())
                {
                    if self.view.search_activity == Some(SearchActivity::Funkwhale) {
                        return;
                    }
                    self.funkwhale.page_turn = Some(self.view.selected);
                    self.begin_search_activity(SearchActivity::Funkwhale);
                    self.queue_funkwhale_job(Job::Search {
                        generation: self.funkwhale.generation,
                        pod,
                        query: self.funkwhale.query.clone(),
                        page,
                        size: self.funkwhale.page_size,
                        artist: self
                            .funkwhale
                            .artist
                            .as_ref()
                            .map(|(identity, _)| identity.clone()),
                        album: self
                            .funkwhale
                            .album
                            .as_ref()
                            .map(|(identity, _)| identity.clone()),
                        tag: self.funkwhale.tag.as_ref().map(|(tag, _)| tag.clone()),
                        standalone: self.funkwhale.standalone,
                        uploader: self
                            .funkwhale
                            .uploader
                            .as_ref()
                            .map(|(actor, _)| actor.clone()),
                    });
                }
                return;
            }
            match self.selected_funkwhale_queue_item() {
                Ok(item) => self.play_queue_item(item, false),
                Err(error) => self.view.status_line = error,
            }
        }
    }

    /// Captures stable page-based replay metadata for queue, notes, commands, and playlists.
    pub(super) fn selected_funkwhale_queue_item(&self) -> Result<QueueItem, String> {
        #[cfg(feature = "funkwhale")]
        {
            if self.funkwhale_artist_directory() {
                return Err("Select a Funkwhale track, not an album".to_owned());
            }
            self.funkwhale
                .items
                .get(self.view.selected)
                .map(queue_item)
                .transpose()?
                .ok_or_else(|| "Select a Funkwhale track".to_owned())
        }
        #[cfg(not(feature = "funkwhale"))]
        {
            Err("This build omits the `funkwhale` feature".to_owned())
        }
    }

    /// Captures the source's existing order without letting later searches replace autoplay.
    #[cfg(feature = "funkwhale")]
    pub(super) fn funkwhale_autoplay_origin(&self, id: &MediaId) -> Option<AutoplayOrigin> {
        let items = self
            .funkwhale
            .items
            .iter()
            .filter_map(|track| queue_item(track).ok())
            .collect::<Vec<_>>();
        let index = items.iter().position(|item| item.media.id == *id)?;
        Some(AutoplayOrigin::Funkwhale {
            items: items.into(),
            index,
        })
    }

    /// Retains one current resolver and rejects obsolete playback completions.
    #[cfg(feature = "funkwhale")]
    pub(super) fn request_funkwhale_playback(
        &mut self,
        item: QueueItem,
        queued: bool,
        origin: Option<AutoplayOrigin>,
    ) {
        self.cancel_funkwhale_playback();
        #[cfg(feature = "soundcloud")]
        self.cancel_pending_soundcloud_playback();
        #[cfg(feature = "yandex-music")]
        self.cancel_pending_yandex_music_playback();
        #[cfg(feature = "yt-dlp")]
        self.cancel_live_start_preparation();
        #[cfg(feature = "bbc-radio")]
        if self.pending_bbc_playback.take().is_some() {
            self.bbc_playback_generation = self.bbc_playback_generation.wrapping_add(1);
            self.clear_playback_start_activity();
        }
        if item.media.id.external_id != item.media.webpage_url.as_str() {
            self.view.status_line =
                "Funkwhale item identity does not match its original page".to_owned();
            return;
        }
        self.remember_funkwhale_playback_retry(&item, origin.as_ref());
        self.view.status_line = "Resolving Funkwhale audio...".to_owned();
        self.queue_funkwhale_job(Job::Playback {
            generation: self.funkwhale.playback_generation,
            item: Box::new(item),
            queued,
            origin,
        });
    }

    /// Invalidates only pending playback; the current accepted stream keeps playing.
    #[cfg(feature = "funkwhale")]
    pub(super) fn cancel_funkwhale_playback(&mut self) {
        self.funkwhale.playback_retry = None;
        self.funkwhale.playback_generation = self.funkwhale.playback_generation.wrapping_add(1);
        self.funkwhale
            .jobs
            .retain(|job| !matches!(job, Job::Playback { .. }));
    }

    /// Finds the current intent without letting an obsolete in-flight request own EOF.
    #[cfg(feature = "funkwhale")]
    fn pending_funkwhale_playback_job(&self) -> Option<&Job> {
        self.funkwhale.jobs.iter().chain(self.funkwhale.worker.iter().map(|worker| &worker.job)).find(|job| matches!(job, Job::Playback { generation, .. } if *generation == self.funkwhale.playback_generation))
    }

    /// A new explicit resolver survives the previous accepted track reaching EOF.
    #[cfg(feature = "funkwhale")]
    pub(super) fn funkwhale_playback_pending(&self) -> bool {
        self.pending_funkwhale_playback_job().is_some()
            || self.funkwhale_playback_retry_deadline().is_some()
    }

    /// Distinguishes an already-positioned queue action from a new insertion after EOF.
    #[cfg(feature = "funkwhale")]
    pub(super) fn funkwhale_playback_owns_queue_cursor(&self) -> bool {
        matches!(
            self.pending_funkwhale_playback_job(),
            Some(Job::Playback { queued: true, .. })
        )
    }

    /// Preserves an edited query while covered, without keeping a hidden search spinner.
    pub(super) fn leave_funkwhale_screen(&mut self) {
        self.funkwhale.query.clone_from(&self.view.search_query);
        self.funkwhale.selected = self.view.selected;
        #[cfg(feature = "funkwhale")]
        {
            self.funkwhale.page_turn = None;
        }
        self.finish_search_activity(SearchActivity::Funkwhale);
    }

    /// Revokes all queued intents; a bounded in-flight read cannot publish UI changes after exit.
    #[cfg(feature = "funkwhale")]
    pub(super) fn shutdown_funkwhale(&mut self) {
        self.cancel_funkwhale_playback();
        self.funkwhale.generation = self.funkwhale.generation.wrapping_add(1);
        self.funkwhale.jobs.clear();
        self.funkwhale.restore = false;
    }

    /// Limits resolved-input admission to the exact current resolver completion.
    #[cfg(feature = "funkwhale")]
    pub(super) fn funkwhale_resolved_candidate(&self, id: &MediaId) -> bool {
        self.funkwhale.admitting.as_ref() == Some(id)
    }

    /// Reveals the playing identity in its own tab without starting playback again.
    #[cfg(feature = "funkwhale")]
    pub(super) fn reveal_funkwhale_now_playing(&mut self, id: &MediaId) -> bool {
        if id.source != SourceKind::Funkwhale {
            return false;
        }
        let Some(index) = self
            .funkwhale
            .items
            .iter()
            .position(|track| track.video_id == id.external_id)
        else {
            return false;
        };
        self.show_screen(Screen::Funkwhale);
        self.view.selected = index;
        self.update_funkwhale_detail();
        true
    }

    /// Retains canonical metadata for replay even after a later search replaces its rows.
    #[cfg(feature = "funkwhale")]
    pub(super) fn funkwhale_navigation_snapshot(
        &self,
        item: &QueueItem,
    ) -> Option<FunkwhaleNavigationSnapshot> {
        FunkwhaleProvider::instance_url_from_track_id(&item.media.id.external_id).ok()?;
        let track = self
            .funkwhale
            .items
            .iter()
            .find(|track| track.video_id == item.media.id.external_id)
            .cloned()
            .unwrap_or_else(|| VideoSummary {
                video_id: item.media.id.external_id.clone(),
                title: item.media.title.clone(),
                channel_name: item.media.creator.clone().unwrap_or_default(),
                channel_id: String::new(),
                description: item.media.description.clone().unwrap_or_default(),
                duration_seconds: item.media.duration_seconds,
                view_count: None,
                published_at: item.media.published_at,
                published_text: None,
                live: false,
                was_live: false,
                orientation: VideoOrientation::Unknown,
                thumbnails: item
                    .media
                    .thumbnail_url
                    .iter()
                    .cloned()
                    .map(|url| crate::providers::Thumbnail {
                        url,
                        quality: Some("original".to_owned()),
                        width: None,
                        height: None,
                    })
                    .collect(),
                webpage_url: Some(item.media.webpage_url.clone()),
                stream_url: None,
            });
        Some(FunkwhaleNavigationSnapshot {
            items: vec![track],
            albums: Vec::new(),
            standalone: false,
            standalone_presence: None,
            next_page: None,
            page_size: self.funkwhale.page_size,
            artist: None,
            album: None,
            tag: None,
            uploader: None,
        })
    }

    /// Swaps only browsing data and rejects old search completions, preserving playback.
    #[cfg(feature = "funkwhale")]
    pub(super) fn swap_funkwhale_navigation(&mut self, snapshot: &mut FunkwhaleNavigationSnapshot) {
        self.funkwhale.generation = self.funkwhale.generation.wrapping_add(1);
        self.funkwhale
            .jobs
            .retain(|job| !matches!(job, Job::Search { .. } | Job::Details { .. }));
        self.funkwhale.page_turn = None;
        self.funkwhale.restore = false;
        self.finish_search_activity(SearchActivity::Funkwhale);
        std::mem::swap(&mut self.funkwhale.items, &mut snapshot.items);
        std::mem::swap(&mut self.funkwhale.albums, &mut snapshot.albums);
        std::mem::swap(&mut self.funkwhale.standalone, &mut snapshot.standalone);
        std::mem::swap(
            &mut self.funkwhale.standalone_presence,
            &mut snapshot.standalone_presence,
        );
        std::mem::swap(&mut self.funkwhale.next_page, &mut snapshot.next_page);
        std::mem::swap(&mut self.funkwhale.page_size, &mut snapshot.page_size);
        std::mem::swap(&mut self.funkwhale.artist, &mut snapshot.artist);
        std::mem::swap(&mut self.funkwhale.album, &mut snapshot.album);
        std::mem::swap(&mut self.funkwhale.tag, &mut snapshot.tag);
        std::mem::swap(&mut self.funkwhale.uploader, &mut snapshot.uploader);
    }

    /// Reports source-local ownership to the shared now-playing Back guard.
    #[cfg(feature = "funkwhale")]
    pub(super) fn funkwhale_navigation_generation(&self) -> u64 {
        self.funkwhale.generation
    }

    /// Replaces like-for-like queued intents rather than creating unbounded HTTP workers.
    #[cfg(feature = "funkwhale")]
    fn queue_funkwhale_job(&mut self, job: Job) {
        if self.diagnostic_only || self.view.quitting {
            return;
        }
        if matches!(job, Job::Details { .. } | Job::Pods)
            && self
                .funkwhale
                .worker
                .as_ref()
                .is_some_and(|worker| same_job(&worker.job, &job))
        {
            return;
        }
        self.funkwhale
            .jobs
            .retain(|old| std::mem::discriminant(old) != std::mem::discriminant(&job));
        if matches!(job, Job::Playback { .. }) {
            self.funkwhale.jobs.push_front(job);
        } else {
            self.funkwhale.jobs.push_back(job);
        }
        self.poll_funkwhale_worker();
    }

    /// Drains notified replies and starts at most one queued bounded HTTP operation.
    pub(super) fn poll_funkwhale_worker(&mut self) {
        #[cfg(feature = "funkwhale")]
        {
            if self.view.quitting || self.shutdown_persistence_succeeded.is_some() {
                return;
            }
            self.poll_due_funkwhale_playback_retry(Instant::now());
            if !self.config.persistence.save_playback_history {
                self.funkwhale.restore = false;
                self.funkwhale.restore_selected = None;
            }
            // The pod's default twenty-track page is not a restored user query:
            // it needs neither terminal geometry nor permission to save history.
            if self.view.screen == Screen::Funkwhale
                && !self.funkwhale.recent_attempted
                && !self.funkwhale.restore
                && self.funkwhale.query.trim().is_empty()
                && self.funkwhale.items.is_empty()
                && self.view.funkwhale_setup_popup.is_none()
                && self.config.providers.funkwhale_instance_url.is_some()
                && !self.view.search_editing
            {
                self.submit_funkwhale_search(String::new());
                return;
            }
            if self.view.screen == Screen::Funkwhale && self.funkwhale.restore {
                self.funkwhale.restore = false;
                if self.config.persistence.save_playback_history
                    && !self.view.search_editing
                    && self.config.providers.funkwhale_instance_url.is_some()
                {
                    let selected = self
                        .funkwhale
                        .restore_selected
                        .take()
                        .unwrap_or(self.funkwhale.selected);
                    self.submit_funkwhale_search(self.funkwhale.query.clone());
                    self.funkwhale.selected = selected;
                    return;
                }
            }
            let completed = self.funkwhale.worker.as_ref().and_then(|worker| {
                match worker.response.try_recv() {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Disconnected) => {
                        Some(Err("Funkwhale worker stopped without a result".to_owned()))
                    }
                    Err(TryRecvError::Empty) => None,
                }
            });
            if let Some(result) = completed {
                let worker = self.funkwhale.worker.take().expect("published worker");
                reap_published_worker(worker.thread);
                self.apply_funkwhale_reply(worker.job, result);
            }
            if self.funkwhale.worker.is_some() {
                return;
            }
            let Some(job) = self.funkwhale.jobs.pop_front() else {
                return;
            };
            let client = match self.funkwhale_client_for_job(&job) {
                Ok(client) => client,
                Err(error) => {
                    self.apply_funkwhale_reply(job, Err(error));
                    return;
                }
            };
            let work = job.clone();
            let (sender, response) = bounded(1);
            let sender = ResponseSender::new(sender, self.worker_notifier.clone());
            match thread::Builder::new()
                .name("youta-funkwhale".to_owned())
                .spawn(move || {
                    let result = run_job(&work, client);
                    let _ = sender.send(result);
                }) {
                Ok(thread) => {
                    self.funkwhale.worker = Some(Worker {
                        job,
                        response,
                        thread,
                    })
                }
                Err(error) => self.apply_funkwhale_reply(job, Err(error.to_string())),
            }
        }
    }

    /// Reuses API-version discovery and HTTP connections across requests to up to four pods.
    #[cfg(feature = "funkwhale")]
    fn funkwhale_client_for_job(&mut self, job: &Job) -> Result<Option<FunkwhaleProvider>, String> {
        let pod = match job {
            Job::Pods => return Ok(None),
            Job::Search { pod, .. } => pod.clone(),
            Job::Details { id, .. } => FunkwhaleProvider::instance_url_from_track_id(id)
                .map_err(|error| error.to_string())?,
            Job::Playback { item, .. } => {
                FunkwhaleProvider::instance_url_from_track_id(&item.media.id.external_id)
                    .map_err(|error| error.to_string())?
            }
        };
        if let Some(index) = self
            .funkwhale
            .clients
            .iter()
            .position(|client| client.base_url() == &pod)
        {
            let client = self
                .funkwhale
                .clients
                .remove(index)
                .expect("cached client index");
            self.funkwhale.clients.push_front(client.clone());
            return Ok(Some(client));
        }
        let client = FunkwhaleProvider::new(pod).map_err(|error| error.to_string())?;
        self.funkwhale.clients.truncate(3);
        self.funkwhale.clients.push_front(client.clone());
        Ok(Some(client))
    }

    /// Bounds selected metadata by canonical track owner and preserves enrichment during playback.
    #[cfg(feature = "funkwhale")]
    fn cache_funkwhale_details(&mut self, mut details: VideoDetails, upload: UploadMetadataUpdate) {
        if self.funkwhale.details.len() >= 128
            && !self.funkwhale.details.contains_key(&details.video_id)
        {
            self.funkwhale.details.clear();
            self.funkwhale.upload_metadata.clear();
            self.funkwhale.upload_metadata_checked.clear();
        }
        if let UploadMetadataUpdate::Replace(upload) = upload {
            self.funkwhale
                .upload_metadata_checked
                .insert(details.video_id.clone());
            if let Some(upload) = upload {
                self.funkwhale
                    .upload_metadata
                    .insert(details.video_id.clone(), upload);
            } else {
                self.funkwhale.upload_metadata.remove(&details.video_id);
            }
        }
        if let Some(upload) = self.funkwhale.upload_metadata.get(&details.video_id) {
            append_upload_metadata(&mut details, upload);
        }
        self.funkwhale
            .details
            .insert(details.video_id.clone(), details);
    }

    /// Accepts only current search, selection, and playback owners, never another tab's metadata.
    #[cfg(feature = "funkwhale")]
    fn apply_funkwhale_reply(&mut self, job: Job, result: Result<Reply, String>) {
        match job {
            Job::Search {
                generation, pod, ..
            } if generation == self.funkwhale.generation => {
                self.finish_search_activity(SearchActivity::Funkwhale);
                match result {
                    Ok(Reply::Albums(page, presence)) if self.funkwhale_artist_directory() => {
                        if let Some(presence) = presence {
                            self.funkwhale.standalone_presence = presence.ok();
                        }
                        let before = self.funkwhale.albums.len();
                        let empty_page = page.items.is_empty();
                        for album in page.items {
                            if self.funkwhale.albums.len() < 1000
                                && !self.funkwhale.albums.iter().any(|old| old.url == album.url)
                            {
                                self.funkwhale.albums.push(album);
                            }
                        }
                        self.funkwhale.next_page = ((self.funkwhale.albums.len() > before
                            || empty_page)
                            && self.funkwhale.albums.len() < 1000)
                            .then_some(page.next_page)
                            .flatten();
                        if self.funkwhale.page_turn.take().is_some_and(|row| {
                            self.view.screen == Screen::Funkwhale && self.funkwhale.selected == row
                        }) {
                            self.funkwhale.selected = if self.funkwhale.next_page.is_some() {
                                self.funkwhale.albums.len()
                                    + usize::from(self.funkwhale_standalone_entry_visible())
                            } else {
                                self.funkwhale.albums.len().saturating_sub(1)
                            };
                        }
                        if self.view.screen == Screen::Funkwhale {
                            self.populate_funkwhale();
                            self.refresh_selected_playlist_state();
                        }
                    }
                    Ok(Reply::Search(page)) if !self.funkwhale_artist_directory() => {
                        let before = self.funkwhale.items.len();
                        let empty_page = page.items.is_empty();
                        for item in page.items {
                            if let SearchItem::Video(track) = item
                                && self.funkwhale.items.len() < 1000
                                && !self
                                    .funkwhale
                                    .items
                                    .iter()
                                    .any(|old| old.video_id == track.video_id)
                            {
                                self.funkwhale.items.push(track);
                            }
                        }
                        self.funkwhale.next_page = ((self.funkwhale.items.len() > before
                            || empty_page)
                            && self.funkwhale.items.len() < 1000)
                            .then_some(page.next_page)
                            .flatten();
                        if self.funkwhale.page_turn.take().is_some_and(|row| {
                            self.view.screen == Screen::Funkwhale && self.funkwhale.selected == row
                        }) {
                            self.funkwhale.selected = if self.funkwhale.next_page.is_some() {
                                self.funkwhale.items.len()
                            } else {
                                self.funkwhale.items.len().saturating_sub(1)
                            };
                        }
                        if self.view.screen == Screen::Funkwhale {
                            self.populate_funkwhale();
                            self.refresh_selected_playlist_state();
                        }
                    }
                    Err(error) => {
                        self.funkwhale.page_turn = None;
                        if self.view.screen == Screen::Funkwhale {
                            let message = format!("Funkwhale pod {pod}: {error}");
                            self.view.status_line.clone_from(&message);
                            self.show_actionable_message("Cannot load Funkwhale tracks", message);
                        }
                    }
                    _ => {}
                }
            }
            Job::Details { generation, id } if generation == self.funkwhale.generation => {
                match result {
                    Ok(Reply::DetailsWithUpload(details, upload)) if details.video_id == id => {
                        self.cache_funkwhale_details(
                            details,
                            UploadMetadataUpdate::Replace(upload),
                        );
                    }
                    Err(error) => {
                        self.funkwhale.failed_details.insert(id.clone());
                        if self.view.screen == Screen::Funkwhale
                            && self
                                .funkwhale
                                .items
                                .get(self.view.selected)
                                .is_some_and(|item| item.video_id == id)
                        {
                            self.view.status_line = format!("Funkwhale track {id}: {error}");
                        }
                    }
                    _ => {
                        self.funkwhale.failed_details.insert(id.clone());
                    }
                }
                if self.view.screen == Screen::Funkwhale
                    && self
                        .funkwhale
                        .items
                        .get(self.view.selected)
                        .is_some_and(|item| item.video_id == id)
                {
                    self.update_funkwhale_detail();
                    self.refresh_selected_playlist_state();
                }
            }
            Job::Playback {
                generation,
                mut item,
                queued,
                origin,
            } if generation == self.funkwhale.playback_generation => match result {
                Ok(Reply::Playback(details, stream))
                    if details.video_id == item.media.id.external_id =>
                {
                    item.media.title = details.title.clone();
                    item.media.description = Some(details.description.clone());
                    item.media.duration_seconds = details.duration_seconds;
                    item.media.license = details
                        .license
                        .clone()
                        .map_or(MediaLicense::Unknown, MediaLicense::Other);
                    self.cache_funkwhale_details(details, UploadMetadataUpdate::Unchanged);
                    let mut input = PlaybackInput::new(stream.to_string());
                    input.bypass_ytdl = true;
                    self.funkwhale.admitting = Some(item.media.id.clone());
                    self.play_queue_item_with_origin_and_input(*item, queued, origin, Some(input));
                    self.funkwhale.admitting = None;
                }
                Err(error) => {
                    if self.retry_funkwhale_resolution_error(&item, &error) {
                        return;
                    }
                    let message = format!("Funkwhale track {}: {error}", item.media.webpage_url);
                    self.view.status_line.clone_from(&message);
                    self.show_actionable_message("Cannot play Funkwhale track", message);
                }
                _ => {
                    let message =
                        "Funkwhale returned a different track; playback was not started".to_owned();
                    self.view.status_line.clone_from(&message);
                    self.show_error_message("Cannot play Funkwhale track", message);
                }
            },
            Job::Pods => match result {
                Ok(Reply::Pods(pods)) => {
                    self.funkwhale.pods = Some(pods);
                    if let Some(picker) = self
                        .view
                        .funkwhale_setup_popup
                        .as_mut()
                        .and_then(|popup| popup.picker.as_mut())
                    {
                        picker.loading = false;
                        picker.pods = self
                            .funkwhale
                            .pods
                            .as_ref()
                            .unwrap()
                            .iter()
                            .map(pod_view)
                            .collect();
                    }
                }
                Err(error) => {
                    if let Some(picker) = self
                        .view
                        .funkwhale_setup_popup
                        .as_mut()
                        .and_then(|popup| popup.picker.as_mut())
                    {
                        picker.loading = false;
                        picker.error = Some(error);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Performs one read-only operation, including the first album page's bounded presence probe.
#[cfg(feature = "funkwhale")]
fn run_job(job: &Job, client: Option<FunkwhaleProvider>) -> Result<Reply, String> {
    let request = || -> Result<Reply, crate::providers::ProviderError> {
        let client = || {
            client.as_ref().ok_or_else(|| {
                crate::providers::ProviderError::InvalidRequest(
                    "Funkwhale operation has no owning pod".to_owned(),
                )
            })
        };
        match job {
            Job::Pods => discover_public_pods().map(Reply::Pods),
            Job::Search {
                query,
                page,
                size,
                artist,
                album,
                tag,
                standalone,
                uploader,
                ..
            } => {
                let provider = client()?.clone().with_page_size(*size);
                if let Some(actor) = uploader {
                    return provider.uploader_tracks(actor, *page).map(Reply::Search);
                }
                if let Some(artist) = artist {
                    return if *standalone {
                        provider
                            .standalone_artist_tracks(artist, *page)
                            .map(Reply::Search)
                    } else {
                        let albums = provider.artist_albums(artist, *page)?;
                        let presence = (*page == 1).then(|| {
                            provider
                                .artist_has_standalone_tracks(artist)
                                .map_err(|error| error.to_string())
                        });
                        Ok(Reply::Albums(albums, presence))
                    };
                }
                if let Some(album) = album {
                    return provider.album_tracks(album, *page).map(Reply::Search);
                }
                if let Some(tag) = tag {
                    return provider.tag_tracks(tag, *page).map(Reply::Search);
                }
                if query.trim().is_empty() {
                    return provider.recent_tracks(*page).map(Reply::Search);
                }
                let mut request = SearchRequest::new(query.clone(), SearchTarget::Videos);
                request.page = *page;
                provider.search(&request).map(Reply::Search)
            }
            Job::Details { id, .. } => client()?
                .video_details_with_upload_metadata(id)
                .map(|(details, upload)| Reply::DetailsWithUpload(details, upload)),
            Job::Playback { item, .. } => {
                let provider = client()?;
                let details = provider.video_details(&item.media.id.external_id)?;
                let stream = details.stream_url.clone().ok_or_else(|| crate::providers::ProviderError::InvalidResponse("This track has no advertised public audio upload; the pod may require an account".to_owned()))?;
                Ok(Reply::Playback(details, stream))
            }
        }
    };
    request().map_err(|error| error.to_string())
}

/// Recognizes duplicate low-priority work already occupying the single request lane.
#[cfg(feature = "funkwhale")]
fn same_job(left: &Job, right: &Job) -> bool {
    match (left, right) {
        (Job::Pods, Job::Pods) => true,
        (
            Job::Details {
                generation: a,
                id: left,
            },
            Job::Details {
                generation: b,
                id: right,
            },
        ) => a == b && left == right,
        _ => false,
    }
}

#[cfg(feature = "funkwhale")]
fn pod_view(pod: &FunkwhalePod) -> FunkwhalePodView {
    FunkwhalePodView {
        label: pod.label.clone(),
        url: pod.url.to_string(),
        open_registrations: pod.open_registrations,
    }
}

/// Identifies the setup vocabulary so modal dispatch cannot reach covered controls.
pub(super) fn setup_action(action: &UiAction) -> bool {
    matches!(
        action,
        UiAction::OpenFunkwhaleSetup
            | UiAction::CloseFunkwhaleSetup
            | UiAction::SetFunkwhalePodUrl(_)
            | UiAction::EditFunkwhalePodUrl(_)
            | UiAction::BackspaceFunkwhalePodUrl
            | UiAction::ClearFunkwhalePodUrl
            | UiAction::SaveFunkwhaleSetup
            | UiAction::OpenFunkwhalePodPicker
            | UiAction::CloseFunkwhalePodPicker
            | UiAction::MoveFunkwhalePod(_)
            | UiAction::SelectFunkwhalePod(_)
    )
}

/// Builds a page-only replay record; HTTP audio resolution remains action-scoped.
#[cfg(feature = "funkwhale")]
fn queue_item(track: &VideoSummary) -> Result<QueueItem, String> {
    let page = track
        .webpage_url
        .clone()
        .ok_or_else(|| "Funkwhale track has no original page".to_owned())?;
    if page.as_str() != track.video_id {
        return Err("Funkwhale track identity does not match its pod page".to_owned());
    }
    Ok(QueueItem {
        media: MediaItem {
            id: MediaId::new(SourceKind::Funkwhale, &track.video_id),
            kind: MediaKind::Audio,
            title: track.title.clone(),
            creator: Some(track.channel_name.clone()),
            description: Some(track.description.clone()),
            webpage_url: page.clone(),
            thumbnail_url: track
                .thumbnails
                .first()
                .map(|thumbnail| thumbnail.url.clone()),
            duration_seconds: track.duration_seconds,
            published_at: track.published_at,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: description_chapters(&track.description, track.duration_seconds),
            captions: Vec::new(),
        },
        playback_location: page.to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    })
}

/// Appends only facts from the advertised upload record, not the credited artist or added date.
#[cfg(feature = "funkwhale")]
fn append_upload_metadata(details: &mut VideoDetails, upload: &FunkwhaleUploadMetadata) {
    let mut lines = Vec::new();
    if let Some(name) = upload.uploader_name.as_deref().or_else(|| {
        upload
            .uploader_actor
            .as_ref()
            .map(FunkwhaleUploader::full_username)
    }) {
        lines.push(if upload.uploader_actor.is_some() {
            format!("Uploaded by [e]: {name}")
        } else {
            format!("Uploaded by: {name}")
        });
        if let Some(url) = upload.uploader_url.as_ref() {
            lines.push(url.to_string());
        }
    }
    if let Some(date) = upload.created_at {
        lines.push(format!("Upload date: {}", format_unix_utc_date(date)));
    }
    if !lines.is_empty() {
        if !details.description.is_empty() {
            details.description.push_str("\n\n");
        }
        details.description.push_str(&lines.join("\n"));
    }
}

/// Extracts one provider-advertised album without accepting another pod or an invented title search.
#[cfg(feature = "funkwhale")]
fn track_album(summary: &VideoSummary, details: Option<&VideoDetails>) -> Option<(String, String)> {
    let pod = FunkwhaleProvider::instance_url_from_track_id(&summary.video_id).ok()?;
    for description in details
        .map(|details| details.description.as_str())
        .into_iter()
        .chain(std::iter::once(summary.description.as_str()))
    {
        let Some(name) = description
            .lines()
            .find_map(|line| line.strip_prefix("Album: "))
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let Some(page) = description
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("Album page: "))
        else {
            continue;
        };
        let album_pod = FunkwhaleProvider::instance_url_from_album_id(page).ok()?;
        if album_pod != pod {
            return None;
        }
        let url = url::Url::parse(page).ok()?;
        let id = url.path().trim_end_matches('/').rsplit('/').next()?;
        if id.parse::<u64>().ok()?.to_string() != id {
            return None;
        }
        return Some((page.to_owned(), name.to_owned()));
    }
    None
}

/// Keeps original track artwork before advertised album alternatives without probing them.
///
/// Large and original images precede smaller crops; renderers try this bounded
/// list only when selected artwork actually needs loading, and deduplicate the
/// visible primary URL themselves (including fullscreen expansion).
#[cfg(feature = "funkwhale")]
fn artwork_candidates(thumbnails: &[crate::providers::Thumbnail]) -> Vec<url::Url> {
    let mut candidates = Vec::new();
    for quality in [
        "large_square_crop",
        "original",
        "album_large_square_crop",
        "album_original",
        "medium_square_crop",
        "album_medium_square_crop",
        "small_square_crop",
        "album_small_square_crop",
        "source",
        "album_source",
    ] {
        for thumbnail in thumbnails {
            if thumbnail.quality.as_deref() == Some(quality)
                && candidates.len() < 8
                && !candidates.contains(&thumbnail.url)
            {
                candidates.push(thumbnail.url.clone());
            }
        }
    }
    candidates
}

/// Distinguishes source metadata from YouTube facts without inventing missing counts.
#[cfg(feature = "funkwhale")]
fn detail_view(summary: &VideoSummary, details: Option<&VideoDetails>) -> DetailView {
    let mut description = details.map_or_else(
        || summary.description.clone(),
        |details| details.description.clone(),
    );
    let thumbnails = details.map_or(summary.thumbnails.as_slice(), |details| {
        details.thumbnails.as_slice()
    });
    let mut links = Vec::new();
    let mut license = details
        .and_then(|details| details.license.clone())
        .unwrap_or_default();
    let license_link = license::link(&license);
    if let Some(link) = license_link.as_ref() {
        license.clone_from(&link.label);
    }
    if let Some(track) = summary.webpage_url.as_ref() {
        links.push(DetailLinkView {
            label: "Track".to_owned(),
            url: track.to_string(),
            ..DetailLinkView::default()
        });
    }
    if let Ok(artist) = url::Url::parse(&summary.channel_id)
        && let Ok(pod) = FunkwhaleProvider::instance_url_from_track_id(&summary.video_id)
        && FunkwhaleProvider::instance_url_from_artist_id(&summary.channel_id).ok() == Some(pod)
    {
        links.push(DetailLinkView {
            label: "Artist".to_owned(),
            url: artist.to_string(),
            internal_target: Some(DetailLinkInternalTarget::FunkwhaleArtist(
                artist.to_string(),
            )),
            ..DetailLinkView::default()
        });
    }
    if let Some((url, name)) = track_album(summary, details) {
        let lines = description.lines().collect::<Vec<_>>();
        let album_name = format!("Album: {name}");
        let album_page = format!("Album page: {url}");
        let name_line = lines.iter().position(|line| *line == album_name);
        let url_line = lines.iter().rposition(|line| *line == album_page);
        description = lines
            .into_iter()
            .enumerate()
            .filter_map(|(index, line)| {
                (Some(index) != name_line && Some(index) != url_line).then_some(line)
            })
            .collect::<Vec<_>>()
            .join("\n");
        links.push(DetailLinkView {
            label: name,
            url: url.clone(),
            internal_target: Some(DetailLinkInternalTarget::FunkwhaleAlbum(url)),
            ..DetailLinkView::default()
        });
    }
    if let Some(link) = license_link {
        links.push(link);
    }
    if let Some(details) = details
        && !details.keywords.is_empty()
    {
        description.push_str("\n\nTags: ");
        for (index, tag) in details.keywords.iter().enumerate() {
            if index > 0 {
                description.push_str(", ");
            }
            let start_byte = description.len();
            description.push_str(tag);
            links.push(DetailLinkView {
                label: tag.clone(),
                presentation: DetailLinkPresentation::LabelOnly,
                internal_target: Some(DetailLinkInternalTarget::FunkwhaleTag(tag.clone())),
                description_range: Some(DetailHighlightRange {
                    start_byte,
                    end_byte: description.len(),
                }),
                ..DetailLinkView::default()
            });
        }
    }
    DetailView {
        media_id: Some(MediaId::new(SourceKind::Funkwhale, &summary.video_id)),
        title: summary.title.clone(),
        source: "Funkwhale".to_owned(),
        channel_name: summary.channel_name.clone(),
        channel_id: summary.channel_id.clone(),
        webpage_url: summary.webpage_url.clone(),
        channel_webpage_url: url::Url::parse(&summary.channel_id).ok(),
        length: details
            .and_then(|details| details.duration_seconds)
            .or(summary.duration_seconds)
            .map_or_else(String::new, format_seconds),
        published: summary
            .published_at
            .map_or_else(String::new, format_unix_utc_date),
        license,
        description,
        links,
        thumbnail_url: thumbnails.first().map(|thumbnail| thumbnail.url.clone()),
        expanded_thumbnail_url: thumbnails
            .iter()
            .find(|thumbnail| thumbnail.quality.as_deref() == Some("original"))
            .or(thumbnails.first())
            .map(|thumbnail| thumbnail.url.clone()),
        thumbnail_fallback_urls: artwork_candidates(thumbnails),
        ..DetailView::default()
    }
}

#[cfg(all(test, feature = "funkwhale"))]
mod tests;
