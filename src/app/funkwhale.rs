//! Pod-scoped Funkwhale browsing and notified, bounded HTTP work.
//!
//! Saved queues contain canonical track pages, never resolved audio locators.
//! Opening setup does not select a pod or fetch a directory implicitly.

#[cfg(feature = "funkwhale")]
use super::worker_notifications::reap_published_worker;
use super::*;

#[cfg(feature = "funkwhale")]
use crate::view::{FunkwhalePodPickerView, FunkwhalePodView, FunkwhaleSetupPopupView};

#[cfg(feature = "funkwhale")]
use crate::providers::funkwhale::{FunkwhalePod, FunkwhaleProvider, discover_public_pods};
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
    details: HashMap<String, VideoDetails>,
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
    admitting: Option<MediaId>,
    #[cfg(feature = "funkwhale")]
    next_page: Option<u32>,
    #[cfg(feature = "funkwhale")]
    capacity: usize,
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

/// Exactly one HTTP operation runs while at most four latest intents wait.
#[cfg(feature = "funkwhale")]
#[derive(Clone)]
enum Job {
    Search {
        generation: u64,
        pod: url::Url,
        query: String,
        page: u32,
        size: usize,
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
    Details(VideoDetails),
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
    next_page: Option<u32>,
    page_size: usize,
}

impl AppController {
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
        self.funkwhale.details.clear();
        self.funkwhale.failed_details.clear();
        self.funkwhale.jobs.clear();
        self.funkwhale.next_page = None;
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
        self.funkwhale.capacity = rows.clamp(1, 100);
    }

    /// Starts a query only on the pod explicitly chosen by the user.
    pub(super) fn submit_funkwhale_search(&mut self, query: String) {
        self.funkwhale.query = query;
        if self.view.screen == Screen::Funkwhale {
            self.view.search_query.clone_from(&self.funkwhale.query);
        }
        #[cfg(feature = "funkwhale")]
        {
            self.funkwhale.restore = false;
            self.funkwhale.restore_selected = None;
            let Some(pod) = self.config.providers.funkwhale_instance_url.clone() else {
                self.open_funkwhale_setup();
                return;
            };
            self.funkwhale.generation = self.funkwhale.generation.wrapping_add(1);
            self.funkwhale.items.clear();
            self.funkwhale.next_page = None;
            self.funkwhale.page_turn = None;
            self.funkwhale.selected = 0;
            self.funkwhale.failed_details.clear();
            self.funkwhale
                .jobs
                .retain(|job| matches!(job, Job::Pods | Job::Playback { .. }));
            self.funkwhale.page_size = if self.funkwhale.capacity == 0 {
                50
            } else {
                self.funkwhale.capacity
            };
            self.populate_funkwhale();
            self.begin_search_activity(SearchActivity::Funkwhale);
            self.queue_funkwhale_job(Job::Search {
                generation: self.funkwhale.generation,
                pod,
                query: self.funkwhale.query.clone(),
                page: 1,
                size: self.funkwhale.page_size,
            });
        }
    }

    /// Projects one compact track per terminal row and an explicit continuation only when advertised.
    pub(super) fn populate_funkwhale(&mut self) {
        #[cfg(feature = "funkwhale")]
        {
            self.view.rows = self
                .funkwhale
                .items
                .iter()
                .map(|track| RowView {
                    media_id: Some(MediaId::new(SourceKind::Funkwhale, &track.video_id)),
                    title: track.title.clone(),
                    subtitle: track.channel_name.clone(),
                    source: "Funkwhale".to_owned(),
                    thumbnail_url: track
                        .thumbnails
                        .first()
                        .map(|thumbnail| thumbnail.url.clone()),
                    compact: true,
                    ..RowView::default()
                })
                .collect();
            hydrate_row_playback_progress(&self.store, &mut self.view.rows);
            if self.funkwhale.next_page.is_some() {
                self.view.rows.push(RowView {
                    title: "Load more tracks...".to_owned(),
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
                    format!(
                        "Funkwhale - {} - {} tracks",
                        pod,
                        self.funkwhale.items.len()
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
            let Some(track) = self.funkwhale.items.get(self.view.selected).cloned() else {
                self.view.details = None;
                return;
            };
            let details = self.funkwhale.details.get(&track.video_id);
            let mut next = detail_view(&track, details);
            preserve_thumbnail_expansion(self.view.details.as_ref(), &mut next);
            self.view.details = Some(next);
            if details.is_none() && !self.funkwhale.failed_details.contains(&track.video_id) {
                self.queue_funkwhale_job(Job::Details {
                    generation: self.funkwhale.generation,
                    id: track.video_id,
                });
            }
        }
    }

    /// Plays a real track or turns exactly one viewport-sized continuation page.
    pub(super) fn activate_funkwhale_selection(&mut self) {
        #[cfg(feature = "funkwhale")]
        {
            if self.view.selected == self.funkwhale.items.len() {
                if let (Some(page), Some(pod)) = (
                    self.funkwhale.next_page,
                    self.config.providers.funkwhale_instance_url.clone(),
                ) {
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
            next_page: None,
            page_size: self.funkwhale.page_size,
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
        std::mem::swap(&mut self.funkwhale.next_page, &mut snapshot.next_page);
        std::mem::swap(&mut self.funkwhale.page_size, &mut snapshot.page_size);
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

    /// Accepts only current search, selection, and playback owners, never another tab's metadata.
    #[cfg(feature = "funkwhale")]
    fn apply_funkwhale_reply(&mut self, job: Job, result: Result<Reply, String>) {
        match job {
            Job::Search {
                generation, pod, ..
            } if generation == self.funkwhale.generation => {
                self.finish_search_activity(SearchActivity::Funkwhale);
                match result {
                    Ok(Reply::Search(page)) => {
                        let before = self.funkwhale.items.len();
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
                        self.funkwhale.next_page = (self.funkwhale.items.len() > before
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
                            self.view.status_line = format!("Funkwhale pod {pod}: {error}");
                        }
                    }
                    _ => {}
                }
            }
            Job::Details { generation, id } if generation == self.funkwhale.generation => {
                match result {
                    Ok(Reply::Details(details)) if details.video_id == id => {
                        if self.funkwhale.details.len() >= 128 {
                            self.funkwhale.details.clear();
                        }
                        self.funkwhale.details.insert(id.clone(), details);
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
                    if self.funkwhale.details.len() >= 128 {
                        self.funkwhale.details.clear();
                    }
                    self.funkwhale
                        .details
                        .insert(details.video_id.clone(), details);
                    let mut input = PlaybackInput::new(stream.to_string());
                    input.bypass_ytdl = true;
                    self.funkwhale.admitting = Some(item.media.id.clone());
                    self.play_queue_item_with_origin_and_input(*item, queued, origin, Some(input));
                    self.funkwhale.admitting = None;
                }
                Err(error) => {
                    self.view.status_line =
                        format!("Funkwhale track {}: {error}", item.media.webpage_url)
                }
                _ => {
                    self.view.status_line =
                        "Funkwhale returned a different track; playback was not started".to_owned()
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

/// Performs one read-only, bounded request off the UI thread.
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
                query, page, size, ..
            } => {
                let mut request = SearchRequest::new(query.clone(), SearchTarget::Videos);
                request.page = *page;
                client()?
                    .clone()
                    .with_page_size(*size)
                    .search(&request)
                    .map(Reply::Search)
            }
            Job::Details { id, .. } => client()?.video_details(id).map(Reply::Details),
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

/// Distinguishes source metadata from YouTube facts without inventing missing counts.
#[cfg(feature = "funkwhale")]
fn detail_view(summary: &VideoSummary, details: Option<&VideoDetails>) -> DetailView {
    let description = details.map_or_else(
        || summary.description.clone(),
        |details| details.description.clone(),
    );
    let thumbnails = details.map_or(summary.thumbnails.as_slice(), |details| {
        details.thumbnails.as_slice()
    });
    let mut links = Vec::new();
    if let Ok(artist) = url::Url::parse(&summary.channel_id) {
        links.push(DetailLinkView {
            label: "Artist".to_owned(),
            url: artist.to_string(),
            ..DetailLinkView::default()
        });
    }
    let description = if let Some(details) = details
        && !details.keywords.is_empty()
    {
        format!("{description}\n\nTags: {}", details.keywords.join(", "))
    } else {
        description
    };
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
        license: details
            .and_then(|details| details.license.clone())
            .unwrap_or_default(),
        description,
        links,
        thumbnail_url: thumbnails.first().map(|thumbnail| thumbnail.url.clone()),
        expanded_thumbnail_url: thumbnails
            .iter()
            .find(|thumbnail| thumbnail.quality.as_deref() == Some("original"))
            .or(thumbnails.first())
            .map(|thumbnail| thumbnail.url.clone()),
        ..DetailView::default()
    }
}

#[cfg(all(test, feature = "funkwhale"))]
mod tests;
