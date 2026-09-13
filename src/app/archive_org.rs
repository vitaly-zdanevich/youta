//! Bounded Internet Archive navigation, exact-file playback, and public reviews.

#[cfg(test)]
#[path = "archive_org_worker_tests.rs"]
mod worker_tests;

use super::*;
use crate::providers::archive_org::{
    ArchiveOrgClient, ArchiveOrgItem, ArchiveOrgItemDetails, ArchiveOrgSearchPage,
    ArchiveOrgSearchRequest, ArchiveOrgTrack,
};

/// Search pages and metadata are bounded independently; navigation never waits on HTTP.
#[derive(Default)]
pub(super) struct ArchiveOrgState {
    client: ArchiveOrgClient,
    items: Vec<ArchiveOrgItem>,
    active: Option<Arc<ArchiveOrgItemDetails>>,
    cache: VecDeque<(String, Result<Arc<ArchiveOrgItemDetails>, String>)>,
    next_page: Option<u32>,
    total: u64,
    generation: u64,
    pending: Option<ArchiveJob>,
    request: Option<ArchiveJob>,
    worker: Option<ArchiveWorker>,
    initialized: bool,
    search_selected: usize,
    message: String,
}

/// A replaceable request retains both result ownership and explicit-open intent.
#[derive(Clone)]
struct ArchiveJob {
    generation: u64,
    kind: ArchiveRequest,
    due: Instant,
}

/// Metadata prefetch and explicit opening share a single response/cache entry.
#[derive(Clone)]
enum ArchiveRequest {
    Search(ArchiveOrgSearchRequest),
    Details { identifier: String, open: bool },
}

/// The only live provider thread owns no UI or persistence references.
struct ArchiveWorker {
    job: ArchiveJob,
    response: Receiver<Result<ArchiveResponse, String>>,
    thread: JoinHandle<()>,
}

/// Results retain their provider identity instead of relying on a selected row number.
enum ArchiveResponse {
    Search(ArchiveOrgSearchPage),
    Details(Arc<ArchiveOrgItemDetails>),
}

impl AppController {
    /// Starts a new search without reusing another tab's query or results.
    pub(super) fn submit_archive_org_search(&mut self, query: String) {
        self.archive_org_search_query = query.trim().to_owned();
        self.archive_org.items.clear();
        self.archive_org.active = None;
        self.archive_org.next_page = None;
        self.archive_org.search_selected = 0;
        self.archive_org_selected = 0;
        self.view.selected = 0;
        self.archive_org.initialized = true;
        self.view.search_editing = false;
        self.queue_archive_request(
            ArchiveRequest::Search(ArchiveOrgSearchRequest {
                query: self.archive_org_search_query.clone(),
                page: 1,
                limit: 50,
            }),
            false,
        );
        self.populate_archive_org();
    }

    /// Coalesces rapid selections to one latest request behind one bounded worker.
    fn queue_archive_request(&mut self, kind: ArchiveRequest, debounce: bool) {
        if let ArchiveRequest::Details { identifier, open } = &kind
            && let Some(pending) = &mut self.archive_org.pending
            && let ArchiveRequest::Details {
                identifier: previous,
                open: previous_open,
            } = &mut pending.kind
            && previous == identifier
        {
            *previous_open |= open;
            if !debounce && let Some(request) = &mut self.archive_org.request {
                request.due = Instant::now();
            }
            return;
        }
        self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
        let job = ArchiveJob {
            generation: self.archive_org.generation,
            kind,
            due: Instant::now()
                + if debounce {
                    Duration::from_millis(200)
                } else {
                    Duration::ZERO
                },
        };
        self.archive_org.pending = Some(job.clone());
        self.archive_org.request = Some(job);
        self.start_archive_worker();
    }

    /// Starts no more than one blocking fetch, after any selection debounce expires.
    fn start_archive_worker(&mut self) {
        if self.archive_org.worker.is_some()
            || self
                .archive_org
                .request
                .as_ref()
                .is_none_or(|job| job.due > Instant::now())
        {
            return;
        }
        let job = self
            .archive_org
            .request
            .take()
            .expect("due Archive.org request");
        let work = job.clone();
        let client = self.archive_org.client.clone();
        let (sender, response) = bounded(1);
        match thread::Builder::new()
            .name("youta-archive".to_owned())
            .spawn(move || {
                let result = match &work.kind {
                    ArchiveRequest::Search(request) => {
                        client.search(request).map(ArchiveResponse::Search)
                    }
                    ArchiveRequest::Details { identifier, .. } => client
                        .item_details(identifier)
                        .map(|details| ArchiveResponse::Details(Arc::new(details))),
                }
                .map_err(|error| error.to_string());
                let _ = sender.send(result);
            }) {
            Ok(thread) => {
                self.archive_org.worker = Some(ArchiveWorker {
                    job,
                    response,
                    thread,
                })
            }
            Err(error) => self.handle_archive_response(
                job,
                Err(format!("Cannot start Archive.org worker: {error}")),
            ),
        }
    }

    /// Polls only finished threads, so network delays cannot stop terminal input.
    pub(super) fn poll_archive_org_worker(&mut self) {
        if self
            .archive_org
            .worker
            .as_ref()
            .is_some_and(|worker| worker.thread.is_finished())
        {
            let worker = self
                .archive_org
                .worker
                .take()
                .expect("finished Archive.org worker");
            let _ = worker.thread.join();
            let result = worker
                .response
                .try_recv()
                .unwrap_or_else(|_| Err("Archive.org worker stopped unexpectedly".to_owned()));
            self.handle_archive_response(worker.job, result);
        }
        self.start_archive_worker();
    }

    /// Stores small metadata snapshots but displays only the latest navigation owner.
    fn handle_archive_response(
        &mut self,
        job: ArchiveJob,
        result: Result<ArchiveResponse, String>,
    ) {
        if let ArchiveRequest::Details { identifier, .. } = &job.kind {
            let cached = match &result {
                Ok(ArchiveResponse::Details(details)) => Some(Ok(Arc::clone(details))),
                Err(error) => Some(Err(error.clone())),
                Ok(ArchiveResponse::Search(_)) => None,
            };
            if let Some(cached) = cached {
                self.archive_org.cache.retain(|(id, _)| id != identifier);
                self.archive_org
                    .cache
                    .push_back((identifier.clone(), cached));
                while self.archive_org.cache.len() > 8 {
                    self.archive_org.cache.pop_front();
                }
            }
        }
        if self
            .archive_org
            .pending
            .as_ref()
            .is_none_or(|pending| pending.generation != job.generation)
        {
            return;
        }
        let owner = self
            .archive_org
            .pending
            .take()
            .expect("current Archive.org owner");
        match result {
            Ok(ArchiveResponse::Search(page)) => {
                let mut seen: HashSet<String> = self
                    .archive_org
                    .items
                    .iter()
                    .map(|item| item.identifier.clone())
                    .collect();
                self.archive_org.items.extend(
                    page.items
                        .into_iter()
                        .filter(|item| seen.insert(item.identifier.clone())),
                );
                self.archive_org.items.truncate(1_000);
                self.archive_org.next_page = if self.archive_org.items.len() < 1_000 {
                    page.next_page
                } else {
                    None
                };
                self.archive_org.total = page.total;
                self.archive_org.message = format!(
                    "{} of {} archive.org items · Enter: open · /: search",
                    self.archive_org.items.len(),
                    page.total
                );
            }
            Ok(ArchiveResponse::Details(details)) => {
                if matches!(owner.kind, ArchiveRequest::Details { open: true, .. }) {
                    self.archive_org.search_selected = self.archive_org_selected;
                    self.archive_org.active = Some(Arc::clone(&details));
                    self.archive_org_selected = 0;
                    if self.view.screen == Screen::ArchiveOrg {
                        self.view.selected = 0;
                    }
                    self.archive_org.message = format!(
                        "{} audio tracks · Enter: play · d: download · Esc: back",
                        details.tracks.len()
                    );
                }
                self.complete_archive_comments(&details);
            }
            Err(error) => {
                self.archive_org.message = format!("Archive.org: {error}");
                // Only explicit, still-visible navigation warrants a modal;
                // background selection prefetches must not interrupt the user.
                if self.view.screen == Screen::ArchiveOrg
                    && matches!(owner.kind, ArchiveRequest::Details { open: true, .. })
                {
                    self.show_actionable_message("Could not open archive.org item", &error);
                }
                if let Some(popup) = &mut self.view.video_comments_popup
                    && popup.source == SourceKind::ArchiveOrg
                    && matches!(&owner.kind, ArchiveRequest::Details { identifier, .. } if popup.video_id == *identifier)
                {
                    popup.state = VideoCommentsPopupState::Error(error);
                }
            }
        }
        if self.view.screen == Screen::ArchiveOrg {
            self.populate_archive_org();
            self.refresh_selected_playlist_state();
        }
    }

    /// Projects the catalogue or one item's tracks, preserving the human item URL.
    pub(super) fn populate_archive_org(&mut self) {
        if self.view.screen != Screen::ArchiveOrg {
            return;
        }
        if !self.archive_org.initialized {
            let restored = self.archive_org_selected;
            self.submit_archive_org_search(self.archive_org_search_query.clone());
            self.archive_org_selected = restored;
            return;
        }
        if !self.view.search_editing {
            self.view
                .search_query
                .clone_from(&self.archive_org_search_query);
            self.view.search_cursor_byte = self.view.search_query.len();
        }
        self.view.rows = if let Some(details) = &self.archive_org.active {
            details
                .tracks
                .iter()
                .map(|track| RowView {
                    media_id: Some(track_id(track)),
                    title: track.title.clone(),
                    subtitle: track
                        .duration_seconds
                        .map_or_else(String::new, format_seconds),
                    source: "archive.org".to_owned(),
                    thumbnail_url: track_artwork_url(&details.item, Some(track)),
                    compact: true,
                    ..RowView::default()
                })
                .collect()
        } else {
            let mut rows: Vec<_> = self
                .archive_org
                .items
                .iter()
                .map(|item| RowView {
                    // Containers deliberately have no playable playlist identity.
                    media_id: None,
                    title: item.title.clone(),
                    subtitle: item.creator.clone().unwrap_or_default(),
                    source: "archive.org".to_owned(),
                    thumbnail_url: item.artwork_url.clone(),
                    compact: true,
                    ..RowView::default()
                })
                .collect();
            if self.archive_org.next_page.is_some() {
                rows.push(RowView {
                    title: "Load more items…".to_owned(),
                    source: "archive.org".to_owned(),
                    compact: true,
                    ..RowView::default()
                });
            }
            rows
        };
        hydrate_row_playback_progress(&self.store, &mut self.view.rows);
        self.view.selected = self
            .archive_org_selected
            .min(self.view.rows.len().saturating_sub(1));
        self.archive_org_selected = self.view.selected;
        self.view.status_line = self.archive_org.message.clone();
        if self.archive_org.pending.as_ref().is_some_and(|job| {
            matches!(
                job.kind,
                ArchiveRequest::Search(_) | ArchiveRequest::Details { open: true, .. }
            )
        }) {
            self.begin_search_activity(SearchActivity::ArchiveOrg);
        } else {
            self.finish_search_activity(SearchActivity::ArchiveOrg);
        }
        self.update_archive_org_detail();
    }

    /// Returns selected metadata, merging counts already supplied by search.
    fn selected_archive_item(&self) -> Option<ArchiveOrgItem> {
        let (mut item, summary) = if let Some(details) = &self.archive_org.active {
            (
                details.item.clone(),
                self.archive_org
                    .items
                    .iter()
                    .find(|item| item.identifier == details.item.identifier),
            )
        } else {
            let summary = self.archive_org.items.get(self.view.selected)?;
            (
                self.cached_archive_details(&summary.identifier)
                    .and_then(Result::ok)
                    .map_or_else(|| summary.clone(), |details| details.item.clone()),
                Some(summary),
            )
        };
        let Some(summary) = summary else {
            return Some(item);
        };
        item.favorite_count = item.favorite_count.or(summary.favorite_count);
        item.download_count = item.download_count.or(summary.download_count);
        item.review_count = item.review_count.or(summary.review_count);
        Some(item)
    }

    /// Persists the catalogue row, not an index into a transient open item.
    pub(super) fn archive_org_catalogue_selection(&self) -> usize {
        if self.archive_org.active.is_some() {
            self.archive_org.search_selected
        } else {
            self.archive_org_selected
        }
    }

    /// Clones only the shared cache handle; item snapshots are bounded by the provider.
    fn cached_archive_details(
        &self,
        identifier: &str,
    ) -> Option<Result<Arc<ArchiveOrgItemDetails>, String>> {
        self.archive_org
            .cache
            .iter()
            .rev()
            .find(|(id, _)| id == identifier)
            .map(|(_, result)| result.clone())
    }

    /// Displays known metadata immediately and debounces only missing metadata.
    pub(super) fn update_archive_org_detail(&mut self) {
        self.archive_org_selected = self.view.selected;
        let selected = self.selected_archive_item();
        if self.archive_org.pending.as_ref().is_some_and(|job| {
            matches!(&job.kind, ArchiveRequest::Details { identifier, .. }
                if selected.as_ref().is_none_or(|item| item.identifier != *identifier))
        }) {
            // Moving away revokes an explicit open even if the new row is cached.
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
            self.finish_search_activity(SearchActivity::ArchiveOrg);
        }
        let Some(item) = selected else {
            self.view.details = None;
            return;
        };
        let track = self
            .archive_org
            .active
            .as_ref()
            .and_then(|details| details.tracks.get(self.view.selected));
        let mut detail = detail_view(&item, track);
        let same_identity = self
            .view
            .details
            .as_ref()
            .and_then(|previous| previous.media_id.as_ref())
            == detail.media_id.as_ref();
        if same_identity && let Some(previous) = self.view.details.as_ref() {
            // Rebuilding file facts must not erase the same item's completed lookup.
            detail.wikidata.clone_from(&previous.wikidata);
            detail.links.extend(
                previous
                    .links
                    .iter()
                    .filter(|link| link.wikidata_item_id.is_some())
                    .cloned(),
            );
            detail
                .wikidata_entities
                .clone_from(&previous.wikidata_entities);
            detail
                .expanded_wikidata_item
                .clone_from(&previous.expanded_wikidata_item);
            detail
                .loading_wikidata_item
                .clone_from(&previous.loading_wikidata_item);
        }
        self.view.details = Some(detail);
        if !same_identity {
            self.view.details_scroll = 0;
            self.view.selected_detail_link = None;
        }
        if self.archive_org.active.is_none()
            && self.cached_archive_details(&item.identifier).is_none()
        {
            self.queue_archive_request(
                ArchiveRequest::Details {
                    identifier: item.identifier,
                    open: false,
                },
                true,
            );
        } else if !same_identity
            || self
                .view
                .details
                .as_ref()
                .is_some_and(|details| details.wikidata.is_empty())
        {
            #[cfg(feature = "wikidata")]
            self.request_wikidata(
                crate::providers::wikidata::WikidataExternalKind::ArchiveOrg,
                &item.identifier,
            );
        }
    }

    /// Opens a container or starts the selected exact file with its track-order snapshot.
    pub(super) fn activate_archive_org_selection(&mut self) {
        if let Some(details) = &self.archive_org.active {
            let index = self.view.selected;
            if let Some(track) = details.tracks.get(index) {
                let item = queue_item(&details.item, track);
                let details = Arc::clone(details);
                self.play_queue_item_with_origin(
                    item,
                    false,
                    Some(AutoplayOrigin::ArchiveOrg { details, index }),
                );
            }
            return;
        }
        if let Some(item) = self.archive_org.items.get(self.view.selected).cloned() {
            match self.cached_archive_details(&item.identifier) {
                Some(Ok(details)) => {
                    self.archive_org.search_selected = self.view.selected;
                    self.archive_org.message = format!(
                        "{} audio tracks · Enter: play · d: download · Esc: back",
                        details.tracks.len()
                    );
                    self.archive_org.active = Some(details);
                    self.archive_org_selected = 0;
                    self.populate_archive_org();
                }
                Some(Err(_)) | None => {
                    // Keep passive errors cached, but explicit user intent can retry.
                    self.archive_org
                        .cache
                        .retain(|(id, _)| id != &item.identifier);
                    self.queue_archive_request(
                        ArchiveRequest::Details {
                            identifier: item.identifier,
                            open: true,
                        },
                        false,
                    );
                    self.begin_search_activity(SearchActivity::ArchiveOrg);
                    self.view.status_line = "Opening archive.org item…".to_owned();
                }
            }
        } else if let Some(page) = self.archive_org.next_page
            && self
                .archive_org
                .pending
                .as_ref()
                .is_none_or(|job| !matches!(job.kind, ArchiveRequest::Search(_)))
        {
            self.queue_archive_request(
                ArchiveRequest::Search(ArchiveOrgSearchRequest {
                    query: self.archive_org_search_query.clone(),
                    page,
                    limit: 50,
                }),
                false,
            );
            self.begin_search_activity(SearchActivity::ArchiveOrg);
        }
    }

    /// Leaves a track list or cancels an explicit pending open without losing search results.
    pub(super) fn go_back_archive_org(&mut self) -> bool {
        let was_open = self.archive_org.active.take().is_some();
        let was_pending = self
            .archive_org
            .pending
            .as_ref()
            .is_some_and(|job| matches!(job.kind, ArchiveRequest::Details { open: true, .. }));
        if !was_open && !was_pending {
            return false;
        }
        self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
        self.archive_org.pending = None;
        self.archive_org.request = None;
        if was_open {
            self.archive_org_selected = self.archive_org.search_selected;
        }
        self.archive_org.message = format!(
            "{} of {} archive.org items · Enter: open · /: search",
            self.archive_org.items.len(),
            self.archive_org.total
        );
        self.populate_archive_org();
        true
    }

    /// Only a selected file, never the containing item, can be queued or downloaded.
    pub(super) fn selected_archive_org_queue_item(&self) -> Result<QueueItem, String> {
        self.archive_org
            .active
            .as_ref()
            .and_then(|details| {
                details
                    .tracks
                    .get(self.view.selected)
                    .map(|track| queue_item(&details.item, track))
            })
            .ok_or_else(|| "Open an archive.org item and select an audio track first".to_owned())
    }

    /// Reads playlist eligibility without cloning an item's potentially long description.
    pub(super) fn selected_archive_org_playlist_identity(&self) -> Option<(MediaId, String)> {
        let track = self
            .archive_org
            .active
            .as_ref()?
            .tracks
            .get(self.view.selected)?;
        Some((track_id(track), track.title.clone()))
    }

    /// Returns the original public item page, not the download URL used by playback.
    pub(super) fn current_archive_org_url(&self) -> Option<String> {
        self.selected_archive_item()
            .map(|item| item.webpage_url.to_string())
    }

    /// Opens cached public reviews without requiring a YouTube provider or API key.
    pub(super) fn open_archive_org_comments(&mut self) {
        if self.view.screen != Screen::ArchiveOrg {
            return;
        }
        let Some(item) = self.selected_archive_item() else {
            return;
        };
        self.view.video_comments_popup = Some(VideoCommentsPopupView {
            source: SourceKind::ArchiveOrg,
            video_id: item.identifier.clone(),
            video_title: item.title,
            state: VideoCommentsPopupState::Loading,
            ..VideoCommentsPopupView::default()
        });
        let cached = self
            .archive_org
            .active
            .as_ref()
            .map(|details| Ok(Arc::clone(details)))
            .or_else(|| self.cached_archive_details(&item.identifier));
        match cached {
            Some(Ok(details)) => self.complete_archive_comments(&details),
            Some(Err(_)) | None => {
                self.archive_org
                    .cache
                    .retain(|(id, _)| id != &item.identifier);
                self.queue_archive_request(
                    ArchiveRequest::Details {
                        identifier: item.identifier,
                        open: false,
                    },
                    false,
                );
            }
        }
    }

    /// A closed popup or one owned by another item cannot be reopened by a late response.
    fn complete_archive_comments(&mut self, details: &ArchiveOrgItemDetails) {
        if self
            .view
            .video_comments_popup
            .as_ref()
            .is_some_and(|popup| {
                popup.source == SourceKind::ArchiveOrg && popup.video_id == details.item.identifier
            })
        {
            let mut popup = video_comments_popup(
                details.item.identifier.clone(),
                details.item.title.clone(),
                details.comments.clone(),
            );
            popup.source = SourceKind::ArchiveOrg;
            self.view.video_comments_popup = Some(popup);
        }
    }
}

/// Exact file identities keep multiple tracks from collapsing into one history entry.
fn track_id(track: &ArchiveOrgTrack) -> MediaId {
    MediaId::new(SourceKind::ArchiveOrg, track.download_url.as_str())
}

/// Uses the selected track's verified waveform instead of the item representative.
/// The provider omits per-track waveforms whenever an actual original cover wins.
fn track_artwork_url(item: &ArchiveOrgItem, track: Option<&ArchiveOrgTrack>) -> Option<url::Url> {
    track
        .and_then(|track| track.waveform_url.as_ref())
        .or(item.artwork_url.as_ref())
        .cloned()
}

/// Constructs a stable queryless file snapshot for playback, playlists, and downloads.
pub(super) fn queue_item(item: &ArchiveOrgItem, track: &ArchiveOrgTrack) -> QueueItem {
    QueueItem {
        media: MediaItem {
            id: track_id(track),
            kind: MediaKind::Audio,
            title: track.title.clone(),
            creator: item.creator.clone(),
            description: item.description.clone(),
            webpage_url: track.download_url.clone(),
            thumbnail_url: track_artwork_url(item, Some(track)),
            duration_seconds: track.duration_seconds,
            published_at: item.published_at,
            statistics: MediaStatistics::default(),
            // Displayed rights are not automatically an upload permission.
            license: item
                .license
                .clone()
                .map_or(MediaLicense::Unknown, MediaLicense::Other),
            chapters: Vec::new(),
            captions: Vec::new(),
        },
        playback_location: track.download_url.to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    }
}

/// Only canonical public Archive.org file addresses bypass media extraction.
pub(super) fn is_direct_audio_url(url: &url::Url) -> bool {
    crate::domain::is_canonical_archive_org_audio_url(url)
}

/// Formats item-level provenance independently of the selected audio file's duration.
fn detail_view(item: &ArchiveOrgItem, track: Option<&ArchiveOrgTrack>) -> DetailView {
    let artwork = track_artwork_url(item, track);
    let mut metadata = Vec::new();
    if let Some(creator) = &item.creator {
        metadata.push(format!("Creator: {creator}"));
    }
    if !item.topics.is_empty() {
        metadata.push(format!("Topics: {}", item.topics.join(", ")));
    }
    if !item.languages.is_empty() {
        metadata.push(format!("Language: {}", item.languages.join(", ")));
    }
    if let Some(size) = item.size_bytes {
        metadata.push(format!("Item Size: {}", human_bytes(size)));
    }
    if let Some(date) = item.published_at {
        metadata.push(format!("Content date: {}", format_unix_utc_date(date)));
    }
    let mut description = metadata.join("\n");
    if let Some(text) = &item.description {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(text);
    }
    let mut links = Vec::new();
    if let Some(uploader) = &item.uploader {
        links.push(DetailLinkView {
            prefix: "Uploader: ".to_owned(),
            label: uploader.name.clone(),
            url: uploader.url.to_string(),
            ..DetailLinkView::default()
        });
    }
    links.extend(item.collections.iter().map(|collection| DetailLinkView {
        prefix: "In collections: ".to_owned(),
        label: collection.name.clone(),
        url: collection.url.to_string(),
        presentation: crate::view::DetailLinkPresentation::UrlOnly,
        ..DetailLinkView::default()
    }));
    if let Some(url) = &item.license_url {
        links.push(DetailLinkView {
            prefix: "License: ".to_owned(),
            label: item.license.clone().unwrap_or_else(|| url.to_string()),
            url: url.to_string(),
            ..DetailLinkView::default()
        });
    }
    DetailView {
        media_id: Some(track.map_or_else(
            || MediaId::new(SourceKind::ArchiveOrg, format!("item:{}", item.identifier)),
            track_id,
        )),
        title: track.map_or_else(|| item.title.clone(), |track| track.title.clone()),
        source: "archive.org".to_owned(),
        webpage_url: Some(item.webpage_url.clone()),
        channel_name: item
            .uploader
            .as_ref()
            .map_or_else(String::new, |uploader| uploader.name.clone()),
        channel_webpage_url: item.uploader.as_ref().map(|uploader| uploader.url.clone()),
        description,
        links,
        length: track
            .and_then(|track| track.duration_seconds)
            .map_or_else(String::new, format_seconds),
        likes: item
            .favorite_count
            .map_or_else(String::new, |count| count.to_string()),
        views: item
            .download_count
            .map_or_else(String::new, |count| count.to_string()),
        comments: item
            .review_count
            .map_or_else(String::new, |count| count.to_string()),
        published: item
            .uploaded_at
            .map_or_else(String::new, format_unix_utc_date),
        license: item.license.clone().unwrap_or_default(),
        thumbnail_url: artwork.clone(),
        expanded_thumbnail_url: artwork,
        ..DetailView::default()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> ArchiveOrgItem {
        serde_json::from_value(serde_json::json!({
            "identifier": "fixture", "title": "An audio collection",
            "creator": "A creator", "description": "Original description",
            "webpage_url": "https://archive.org/details/fixture",
            "artwork_url": "https://archive.org/services/img/fixture",
            "uploaded_at": 1_700_000_000, "published_at": 1_600_000_000,
            "favorite_count": 12, "review_count": 3, "download_count": 45,
            "size_bytes": 8192, "topics": ["History"], "languages": ["eng"],
            "license": "CC BY 4.0", "license_url": "https://creativecommons.org/licenses/by/4.0/",
            "uploader": {"name": "Uploader", "url": "https://archive.org/details/@uploader"},
            "collections": [{"name": "Audio collection", "url": "https://archive.org/details/audio"}]
        })).unwrap()
    }

    fn track() -> ArchiveOrgTrack {
        ArchiveOrgTrack {
            waveform_url: None,
            filename: "01 chapter.opus".into(),
            title: "First chapter".into(),
            download_url: url::Url::parse("https://archive.org/download/fixture/01%20chapter.opus")
                .unwrap(),
            duration_seconds: Some(123),
            size_bytes: Some(1024),
        }
    }

    /// Downloading a selected Archive file preserves its URL and existing encoding.
    #[cfg(feature = "yt-dlp")]
    #[test]
    fn selected_archive_audio_download_uses_exact_file_without_conversion() {
        struct Capture(Arc<Mutex<Vec<DownloadRequest>>>);
        impl DownloadLauncher for Capture {
            fn start(
                &mut self,
                request: &DownloadRequest,
            ) -> Result<Box<dyn RunningDownload>, String> {
                self.0.lock().unwrap().push(request.clone());
                Err("fixture captures requests without starting a child".to_owned())
            }
        }
        for extension in ["mp3", "flac"] {
            let directory = crate::test_support::canonical_tempdir("exact Archive download");
            let config = Config::for_dir(directory.path().join("config"));
            let store = StateStore::open_in_memory().unwrap();
            let mut controller = AppController::new(config, store, None, None);
            let requests = Arc::new(Mutex::new(Vec::new()));
            controller.download_launcher = Box::new(Capture(Arc::clone(&requests)));
            let mut selected = track();
            selected.filename = format!("chapter.{extension}");
            selected.download_url = url::Url::parse(&format!(
                "https://archive.org/download/fixture/chapter.{extension}"
            ))
            .unwrap();
            let expected = selected.download_url.clone();
            controller.archive_org.active = Some(Arc::new(ArchiveOrgItemDetails {
                item: item(),
                tracks: vec![selected],
                comments: Vec::new(),
            }));
            controller.view.screen = Screen::ArchiveOrg;
            controller.view.selected = 0;
            controller.start_selected_download();
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].source_url, expected);
            assert_eq!(requests[0].format, DownloadFormat::ExactFile);
            assert_eq!(requests[0].scope, DownloadScope::SingleItem);
            assert!(controller.active_download.is_none());
        }
    }

    #[test]
    fn selected_archive_track_uses_its_own_waveform_in_details_and_history() {
        let image =
            "https://iiif.archive.org/image/iiif/3/fixture%2Fsecond.png/full/max/0/default.jpg";
        let mut value = serde_json::to_value(track()).unwrap();
        value["waveform_url"] = serde_json::json!(image);
        let selected = serde_json::from_value(value).unwrap();
        let item = item();
        let details = detail_view(&item, Some(&selected));
        assert_eq!(
            details.thumbnail_url.as_ref().map(url::Url::as_str),
            Some(image)
        );
        assert_eq!(details.expanded_thumbnail_url, details.thumbnail_url);
        assert_eq!(
            queue_item(&item, &selected).media.thumbnail_url,
            details.thumbnail_url,
            "saved playback must keep the selected track's waveform"
        );
        assert_eq!(detail_view(&item, None).thumbnail_url, item.artwork_url);
    }

    #[test]
    fn file_identity_download_and_replay_never_target_the_item_container() {
        let queue = queue_item(&item(), &track());
        assert_eq!(queue.media.webpage_url.as_str(), queue.playback_location);
        assert_eq!(queue.media.id.source, SourceKind::ArchiveOrg);
        assert!(is_direct_audio_url(&queue.media.webpage_url));
        assert_eq!(
            history_replay_locator(&queue).as_deref(),
            Some(queue.playback_location.as_str())
        );
        assert_eq!(
            detail_view(&item(), Some(&track()))
                .webpage_url
                .unwrap()
                .path(),
            "/details/fixture"
        );
    }

    #[test]
    fn archive_details_keep_uploaded_date_license_and_collection_links() {
        let detail = detail_view(&item(), Some(&track()));
        assert_eq!(detail.webpage_url, Some(item().webpage_url));
        assert!(
            detail
                .links
                .iter()
                .all(|link| link.prefix != "Original item: ")
        );
        assert_eq!(detail.likes, "12");
        assert_eq!(detail.published, format_unix_utc_date(1_700_000_000));
        assert_eq!(detail.license, "CC BY 4.0");
        for text in ["Topics: History", "Language: eng", "Item Size: 8.00 KiB"] {
            assert!(
                detail.description.contains(text),
                "{text}: {}",
                detail.description
            );
        }
        assert!(
            detail
                .links
                .iter()
                .filter(|link| link.prefix == "In collections: ")
                .all(|link| { link.presentation == crate::view::DetailLinkPresentation::UrlOnly })
        );
        assert!(
            detail
                .links
                .iter()
                .any(|link| link.prefix == "In collections: " && link.label == "Audio collection")
        );
        assert!(
            detail
                .links
                .iter()
                .any(|link| link.prefix == "License: " && link.url.contains("creativecommons.org"))
        );
    }

    #[test]
    fn archive_details_omit_a_missing_license_instead_of_inventing_a_placeholder() {
        let mut item = item();
        item.license = None;
        item.license_url = None;
        let detail = detail_view(&item, Some(&track()));
        assert!(detail.license.is_empty());
        assert!(detail.links.iter().all(|link| link.prefix != "License: "));
    }

    #[test]
    fn direct_file_bypass_rejects_pages_other_hosts_and_credentials() {
        for url in [
            "https://archive.org/details/fixture",
            "https://example.org/download/fixture/track.opus",
            "https://user@archive.org/download/fixture/track.opus",
            "https://archive.org/download/fixture/track.opus?token=secret",
            "http://archive.org/download/fixture/track.opus",
            "https://archive.org/download/fixture/",
        ] {
            assert!(
                !is_direct_audio_url(&url::Url::parse(url).unwrap()),
                "{url}"
            );
        }
    }
}
