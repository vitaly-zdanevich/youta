//! Bounded Internet Archive navigation, exact-file playback, and public reviews.

#[cfg(test)]
#[path = "archive_org_worker_tests.rs"]
mod worker_tests;

mod history;
mod playback_choice;
mod session;

#[cfg(test)]
mod search_pagination_tests;

#[cfg(test)]
mod restart_tests;

pub(super) use playback_choice::{ArchivePlaybackOwner, playback_step};

/// Installs bounded metadata without a worker for playback-controller regressions.
#[cfg(test)]
pub(super) fn set_playback_test_details(
    controller: &mut AppController,
    details: Option<Arc<ArchiveOrgItemDetails>>,
) {
    controller.archive_org.active = details;
    controller.archive_org.initialized = true;
}

use super::*;
use crate::domain::ArchiveOrgSearchScope;
use crate::providers::archive_org::ArchiveOrgDownloadVariant;
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
    search_highlighter: super::archive_org_highlight::ArchiveSearchHighlighter,
    description_urls: ArchiveDescriptionUrlCache,
    /// Owns the displayed result set; persisted editor drafts may change separately.
    submitted_query: String,
    /// Metadata field owning the displayed results and their continuations.
    submitted_scope: ArchiveOrgSearchScope,
    /// Latest frontend capacity; absent for frontends that do not report terminal rows.
    search_page_capacity: Option<usize>,
    /// Fixed stride for this result set, independent of subsequent terminal resizes.
    page_limit: Option<usize>,
    /// Explicit continuation owner and old boundary; never steal a changed selection.
    page_turn: Option<(u64, usize)>,
    next_page: Option<u32>,
    total: u64,
    generation: u64,
    pending: Option<ArchiveJob>,
    request: Option<ArchiveJob>,
    worker: Option<ArchiveWorker>,
    initialized: bool,
    download_lookup: Option<ArchiveDownloadLookup>,
    playback_choice_generation: u64,
    playback_choice: Option<playback_choice::PendingArchivePlaybackChoice>,
    search_selected: usize,
    message: String,
    history: VecDeque<history::ArchiveLocation>,
    restoring: Option<history::ArchiveLocation>,
    /// Unconsumed public restart destination; retained until the first Archive tick.
    restart: Option<crate::domain::ArchiveOrgSessionLocation>,
}

/// Retains only the current description projection, independently of its search query.
#[derive(Default)]
struct ArchiveDescriptionUrlCache {
    source: String,
    escapes: Vec<crate::view::DetailUrlEscapeView>,
    #[cfg(test)]
    scans: usize,
}

impl ArchiveDescriptionUrlCache {
    /// Produces display-only mappings without changing authoritative description bytes.
    fn apply(&mut self, details: &mut DetailView) {
        if details.description.len() > crate::links::MAX_URL_DISPLAY_SOURCE_BYTES {
            self.source.clear();
            self.escapes.clear();
            details.description_url_escapes.clear();
            return;
        }
        if self.source != details.description {
            self.source.clone_from(&details.description);
            self.escapes = crate::links::description_url_escapes(&details.description);
            #[cfg(test)]
            {
                self.scans += 1;
            }
        }
        details.description_url_escapes.clone_from(&self.escapes);
    }
}

/// Manual download ownership is independent of the selected tab, item or row.
struct ArchiveDownloadLookup {
    #[cfg(any(feature = "yt-dlp", test))]
    source: url::Url,
    identifier: String,
    generation: u64,
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
    /// Updates future searches without changing in-flight or cached page offsets.
    pub(super) fn update_archive_org_search_page_capacity(&mut self, rows: usize) {
        // Match the provider's existing bounded rows range.
        self.archive_org.search_page_capacity = Some(rows.clamp(1, 100));
    }

    /// Resolves exact existing files for a manual download without changing navigation.
    ///
    /// None means the existing bounded metadata worker is still loading this
    /// source. Active/cache hits avoid HTTP. Legacy empty inventories refresh
    /// once, while errors stay terminal until explicit cancellation or a new
    /// source; polling cannot trigger a retry loop or silently select a file.
    #[cfg(any(feature = "yt-dlp", test))]
    pub(super) fn archive_download_variants(
        &mut self,
        source: &url::Url,
    ) -> Result<Option<Vec<ArchiveOrgDownloadVariant>>, String> {
        let identifier = archive_download_identifier(source)
            .ok_or_else(|| "Expected a canonical public archive.org file URL".to_owned())?;
        if self
            .archive_org
            .download_lookup
            .as_ref()
            .is_some_and(|lookup| lookup.source != *source)
        {
            self.cancel_archive_download_lookup();
        }
        let cached = self.cached_archive_details(&identifier);
        let active = self
            .archive_org
            .active
            .as_ref()
            .filter(|details| details.item.identifier == identifier);
        for details in cached
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .into_iter()
            .chain(active)
        {
            if let Some(track) = details.tracks.iter().find(|track| {
                track.download_url == *source
                    || track
                        .download_variants
                        .iter()
                        .any(|variant| variant.download_url == *source)
            }) && !track.download_variants.is_empty()
            {
                return Ok(Some(track.download_variants.clone()));
            }
        }
        if self.archive_org.download_lookup.is_some() {
            if let Some(result) = cached {
                return match result {
                    Err(error) => Err(error),
                    Ok(_) => Err(
                        "The selected archive.org file has no available download variants"
                            .to_owned(),
                    ),
                };
            }
            return if self.archive_download_lookup_pending() {
                Ok(None)
            } else {
                Err("Archive.org download metadata lookup was canceled".to_owned())
            };
        }
        // A new user action may refresh a failed/legacy cache once. The pinned
        // generation keeps subsequent polls from clearing this attempt's result.
        self.archive_org.cache.retain(|(id, _)| id != &identifier);
        let existing = self.archive_org.pending.as_mut().filter(|job| {
            matches!(&job.kind, ArchiveRequest::Details { identifier: pending, .. } if *pending == identifier)
        });
        let generation = if let Some(job) = existing {
            if let ArchiveRequest::Details { open, .. } = &mut job.kind {
                *open = false;
            }
            if let Some(request) = &mut self.archive_org.request
                && request.generation == job.generation
                && let ArchiveRequest::Details { open, .. } = &mut request.kind
            {
                *open = false;
            }
            job.generation
        } else {
            self.archive_org.generation.wrapping_add(1)
        };
        self.finish_search_activity(SearchActivity::ArchiveOrg);
        self.archive_org.download_lookup = Some(ArchiveDownloadLookup {
            source: source.clone(),
            identifier: identifier.clone(),
            generation,
        });
        self.queue_archive_request(
            ArchiveRequest::Details {
                identifier,
                open: false,
            },
            false,
        );
        self.update_archive_back_available();
        Ok(None)
    }

    /// Identifies only the still-pending manual owner, not a completed popup result.
    fn archive_download_lookup_pending(&self) -> bool {
        self.archive_org
            .download_lookup
            .as_ref()
            .is_some_and(|lookup| {
                self.archive_org
                    .pending
                    .as_ref()
                    .is_some_and(|job| job.generation == lookup.generation)
            })
    }

    /// Revokes a manual lookup without canceling another navigation owner's request.
    /// An in-flight HTTP request can finish into the bounded cache but never opens an item.
    #[cfg(any(feature = "yt-dlp", test))]
    pub(super) fn cancel_archive_download_lookup(&mut self) {
        if self.archive_download_lookup_pending() {
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
        }
        self.archive_org.download_lookup = None;
        self.update_archive_back_available();
    }

    /// Starts a new search without reusing another tab's query or results.
    pub(super) fn submit_archive_org_search(&mut self, query: String) {
        self.archive_org.history.clear();
        self.archive_org.restoring = None;
        self.start_archive_org_search(query, ArchiveOrgSearchScope::Text);
    }

    /// Replaces the previous search with one validated metadata value.
    pub(super) fn search_archive_metadata(&mut self, query: String, scope: ArchiveOrgSearchScope) {
        let request = ArchiveOrgSearchRequest {
            query: query.trim().to_owned(),
            scope,
            page: 1,
            limit: self.archive_org.search_page_capacity.unwrap_or(50),
        };
        if let Err(error) = request.validate() {
            self.view.status_line = error.to_string();
            return;
        }
        // Avoid an empty initial browse before the requested metadata search.
        self.archive_org.initialized = true;
        if self.view.screen != Screen::ArchiveOrg {
            self.show_screen(Screen::ArchiveOrg);
        }
        self.remember_archive_location();
        self.start_archive_org_search(request.query, scope);
    }

    /// Owns accepted text and field independently of any subsequent editor draft.
    fn start_archive_org_search(&mut self, query: String, scope: ArchiveOrgSearchScope) {
        let limit = self.archive_org.search_page_capacity.unwrap_or(50);
        self.start_archive_org_search_with_limit(query, scope, limit);
    }

    /// Pins one page stride for a fresh search or bounded restoration of evicted results.
    fn start_archive_org_search_with_limit(
        &mut self,
        query: String,
        scope: ArchiveOrgSearchScope,
        limit: usize,
    ) {
        self.archive_org.restart = None;
        self.archive_org.page_limit = Some(limit);
        self.archive_org.page_turn = None;
        self.archive_org_search_query = query.trim().to_owned();
        self.archive_org_search_scope = scope;
        self.archive_org.submitted_scope = scope;
        self.archive_org
            .submitted_query
            .clone_from(&self.archive_org_search_query);
        self.archive_org.items.clear();
        self.archive_org.active = None;
        self.archive_org.next_page = None;
        self.archive_org.search_selected = 0;
        self.archive_org_selected = 0;
        self.view.selected = 0;
        self.view.selected_detail_link = None;
        self.view.detail_link_reveal = None;
        self.archive_org.initialized = true;
        self.view.search_editing = false;
        self.queue_archive_request(
            ArchiveRequest::Search(ArchiveOrgSearchRequest {
                scope,
                query: self.archive_org_search_query.clone(),
                page: 1,
                limit,
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
        // Restored tabs wait until the frontend has had a frame to report its
        // result capacity. Frontends without a hint retain the default size.
        if self.view.screen == Screen::ArchiveOrg && !self.archive_org.initialized {
            self.populate_archive_org();
        }
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
        if self.archive_org.download_lookup.as_ref().is_some_and(|lookup| {
            lookup.generation == owner.generation
                && matches!(&owner.kind, ArchiveRequest::Details { identifier, .. } if *identifier == lookup.identifier)
        }) {
            // The download popup polls the cache using its pinned file URL.
            // Do not open an item or rewrite another tab's rows/status/details.
            self.update_archive_back_available();
            return;
        }
        match result {
            Ok(ArchiveResponse::Search(page)) => {
                let previous_len = self.archive_org.items.len();
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
                if let Some((generation, boundary)) = self.archive_org.page_turn.take()
                    && generation == owner.generation
                    && self.archive_org.restoring.is_none()
                    && self.archive_org_selected == boundary
                    && self.archive_org.items.len() > previous_len
                {
                    // A page contains exactly one viewport's items; selecting its
                    // continuation scrolls that new batch into view without gaps.
                    // The final page has no continuation: select its last real row.
                    self.archive_org_selected = if self.archive_org.next_page.is_some() {
                        self.archive_org.items.len()
                    } else {
                        self.archive_org.items.len().saturating_sub(1)
                    };
                    if self.view.screen == Screen::ArchiveOrg {
                        self.view.selected = self.archive_org_selected;
                    }
                }
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
                self.archive_org.page_turn = None;
                let was_restoring = self.archive_org.restoring.is_some();
                self.archive_org.restoring = None;
                self.archive_org.message = if was_restoring {
                    format!(
                        "Saved archive.org location is unavailable; showing the catalogue: {error}"
                    )
                } else {
                    format!("Archive.org: {error}")
                };
                // Only explicit, still-visible navigation warrants a modal;
                // background selection prefetches must not interrupt the user.
                if self.view.screen == Screen::ArchiveOrg
                    && !was_restoring
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
        if self.continue_archive_restore() {
            self.refresh_selected_playlist_state();
            return;
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
        if self.continue_archive_restore() {
            return;
        }
        if !self.archive_org.initialized {
            if self.begin_archive_session_restore() {
                return;
            }
            let restored = self.archive_org_selected;
            self.start_archive_org_search(
                self.archive_org_search_query.clone(),
                self.archive_org_search_scope,
            );
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
        self.update_archive_back_available();
        if self.archive_org.restoring.is_some() {
            // Automatic page/item restoration owns selection until completion;
            // passive prefetch must not replace its pending request.
            self.view.details = None;
        } else {
            self.update_archive_org_detail();
        }
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

    /// Persists the logical catalogue row, never a file or temporary restoration row.
    pub(super) fn archive_org_catalogue_selection(&self) -> usize {
        if let Some(location) = self.archive_org_session_location() {
            return location.catalogue_selected;
        }
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
        if !self.archive_download_lookup_pending()
            && self.archive_org.pending.as_ref().is_some_and(|job| {
                matches!(&job.kind, ArchiveRequest::Details { identifier, .. }
                if selected.as_ref().is_none_or(|item| item.identifier != *identifier))
            })
        {
            // Moving away revokes an explicit open even if the new row is cached.
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
            self.finish_search_activity(SearchActivity::ArchiveOrg);
        }
        self.update_archive_back_available();
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
        if self.archive_org.active.is_none()
            && self.cached_archive_details(&item.identifier).is_none()
        {
            // Search only advertises the small item tile. Wait for metadata to
            // choose the native waveform or cover before requesting artwork.
            detail.thumbnail_url = None;
            detail.expanded_thumbnail_url = None;
        }
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
        // Passive metadata enrichment must not close the same item's artwork modal.
        preserve_thumbnail_expansion(self.view.details.as_ref(), &mut detail);
        self.archive_org.description_urls.apply(&mut detail);
        self.archive_org
            .search_highlighter
            .apply(&self.archive_org.submitted_query, &mut detail);
        self.view.details = Some(detail);
        if !same_identity {
            self.view.details_scroll = 0;
            self.view.selected_detail_link = None;
            self.view.detail_link_reveal = None;
        }
        // Passive selection must not replace an explicitly requested search
        // page. Its response will refresh the current selection's metadata.
        if self
            .archive_org
            .pending
            .as_ref()
            .is_some_and(|job| matches!(job.kind, ArchiveRequest::Search(_)))
        {
            return;
        }
        if !self.archive_download_lookup_pending()
            && self.archive_org.active.is_none()
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
        self.cancel_archive_restore_for_selection();
        if let Some(details) = &self.archive_org.active {
            let index = self.view.selected;
            self.begin_archive_playback(
                Arc::clone(details),
                index,
                self.config.playback.archive_format,
                ArchivePlaybackOwner::Append,
            );
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
                    self.update_archive_back_available();
                }
            }
        } else if let Some(page) = self.archive_org.next_page
            && self
                .archive_org
                .pending
                .as_ref()
                .is_none_or(|job| !matches!(job.kind, ArchiveRequest::Search(_)))
        {
            let boundary = self.archive_org.items.len();
            self.archive_org_selected = self.view.selected;
            self.queue_archive_request(
                ArchiveRequest::Search(ArchiveOrgSearchRequest {
                    scope: self.archive_org.submitted_scope,
                    query: self.archive_org.submitted_query.clone(),
                    page,
                    limit: self.archive_org.page_limit.unwrap_or(50),
                }),
                false,
            );
            self.archive_org.page_turn = self
                .archive_org
                .pending
                .as_ref()
                .map(|job| (job.generation, boundary));
            self.begin_search_activity(SearchActivity::ArchiveOrg);
        }
    }

    /// Leaves a track list or cancels an explicit pending open without losing search results.
    pub(super) fn go_back_archive_org(&mut self) -> bool {
        if self.archive_download_lookup_pending() {
            return false;
        }
        if self.archive_org.restoring.is_some() && !self.archive_org.history.is_empty() {
            return self.restore_archive_location();
        }
        let was_open = self.archive_org.active.take().is_some();
        let was_pending = self
            .archive_org
            .pending
            .as_ref()
            .is_some_and(|job| matches!(job.kind, ArchiveRequest::Details { open: true, .. }));
        if !was_open && !was_pending {
            return self.restore_archive_location();
        }
        self.archive_org.restoring = None;
        self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
        self.archive_org.pending = None;
        self.archive_org.request = None;
        if was_open {
            self.archive_org_selected = self.archive_org.search_selected;
        }
        self.view.selected_detail_link = None;
        self.view.detail_link_reveal = None;
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

/// Validates canonical route spelling before requesting bounded item metadata.
///
/// Decode each filename segment exactly once, reject separators/control bytes,
/// and rebuild with the same URL builder as the provider. This supports Unicode
/// and literal percent signs without accepting encoded traversal or credentials.
#[cfg(any(feature = "yt-dlp", test))]
fn archive_download_identifier(source: &url::Url) -> Option<String> {
    // The provider filename is at most 2048 UTF-8 bytes, escaped at most 3x.
    if source.as_str().len() > 3 * 2048 + 200 || !is_direct_audio_url(source) {
        return None;
    }
    let mut segments = source.path_segments()?;
    if segments.next()? != "download" {
        return None;
    }
    let identifier = segments.next()?;
    if identifier.is_empty()
        || identifier.len() > 100
        || !(identifier.as_bytes()[0].is_ascii_alphanumeric() || identifier.starts_with('@'))
        || !identifier
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return None;
    }
    let mut filenames = Vec::new();
    let mut filename_bytes = 0_usize;
    for segment in segments {
        let mut decoded = Vec::with_capacity(segment.len());
        let mut bytes = segment.as_bytes().iter().copied();
        while let Some(byte) = bytes.next() {
            decoded.push(if byte == b'%' {
                let first = char::from(bytes.next()?).to_digit(16)?;
                let second = char::from(bytes.next()?).to_digit(16)?;
                u8::try_from(first * 16 + second).ok()?
            } else {
                byte
            });
        }
        let filename = String::from_utf8(decoded).ok()?;
        if matches!(filename.as_str(), "" | "." | "..")
            || filename
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return None;
        }
        filename_bytes =
            filename_bytes.checked_add(filename.len() + usize::from(!filenames.is_empty()))?;
        filenames.push(filename);
        if filenames.len() > 32 || filename_bytes > 2048 {
            return None;
        }
    }
    if filenames.is_empty() {
        return None;
    }
    let mut canonical = url::Url::parse("https://archive.org/").ok()?;
    canonical
        .path_segments_mut()
        .ok()?
        .clear()
        .extend(["download", identifier])
        .extend(&filenames);
    (canonical == *source).then(|| identifier.to_owned())
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
    let mut inline_links = Vec::new();
    let creators: Vec<_> = if item.creators.is_empty() {
        item.creator.as_deref().into_iter().collect()
    } else {
        item.creators.iter().map(String::as_str).collect()
    };
    append_searchable_metadata(
        &mut metadata,
        &mut inline_links,
        "Creator: ",
        &creators,
        "; ",
        DetailLinkInternalTarget::ArchiveCreator,
    );
    append_searchable_metadata(
        &mut metadata,
        &mut inline_links,
        "Topics: ",
        &item.topics.iter().map(String::as_str).collect::<Vec<_>>(),
        ", ",
        DetailLinkInternalTarget::ArchiveTopic,
    );
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
            internal_target: crate::providers::archive_org::uploader_search_identity(&uploader.url)
                .map(DetailLinkInternalTarget::ArchiveUploader),
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
    links.extend(inline_links);
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

/// Adds independently actionable metadata values without duplicating their text.
fn append_searchable_metadata(
    metadata: &mut Vec<String>,
    links: &mut Vec<DetailLinkView>,
    prefix: &str,
    values: &[&str],
    separator: &str,
    target: fn(String) -> DetailLinkInternalTarget,
) {
    if values.is_empty() {
        return;
    }
    let mut offset = metadata.iter().map(|line| line.len() + 1).sum::<usize>() + prefix.len();
    for value in values {
        // Long labels remain fully visible but cannot exceed the provider's
        // bounded search contract merely by being clicked.
        if !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control) {
            links.push(DetailLinkView {
                label: (*value).to_owned(),
                internal_target: Some(target((*value).to_owned())),
                description_range: Some(crate::view::DetailHighlightRange {
                    start_byte: offset,
                    end_byte: offset + value.len(),
                }),
                ..DetailLinkView::default()
            });
        }
        offset += value.len() + separator.len();
    }
    metadata.push(format!("{prefix}{}", values.join(separator)));
}
#[cfg(test)]
mod tests {
    use super::*;

    /// URL readability is a cached display projection, never a rewrite of full metadata.
    #[test]
    fn archive_url_display_preserves_full_description_and_reuses_projection() {
        let (_temporary, mut app) = lookup_controller();
        app.view.screen = Screen::ArchiveOrg;
        let mut details = (*lookup_details("fixture")).clone();
        let raw = format!(
            "{} https://commons.wikimedia.org/wiki/File:%C4%90_%D0%AF.png END",
            "x".repeat(60_000)
        );
        details.item.description = Some(raw.clone());
        app.archive_org.active = Some(Arc::new(details));
        app.populate_archive_org();
        let projected = app.view.details.as_ref().unwrap();
        assert!(projected.description.ends_with(&raw));
        assert!(!projected.description_url_escapes.is_empty());
        assert!(
            projected
                .description_url_escapes
                .iter()
                .any(|escape| escape.text == "Đ")
        );
        assert!(
            projected
                .description_url_escapes
                .iter()
                .any(|escape| escape.text == "Я")
        );
        let escapes = projected.description_url_escapes.clone();
        assert_eq!(app.archive_org.description_urls.scans, 1);
        app.update_archive_org_detail();
        app.archive_org.submitted_query = "changed highlight query".into();
        app.update_archive_org_detail();
        assert_eq!(app.archive_org.description_urls.scans, 1);
        assert_eq!(
            app.view.details.as_ref().unwrap().description_url_escapes,
            escapes
        );
        Arc::make_mut(app.archive_org.active.as_mut().unwrap())
            .item
            .description = Some("No encoded links now".into());
        app.update_archive_org_detail();
        assert_eq!(app.archive_org.description_urls.scans, 2);
        assert!(
            app.view
                .details
                .as_ref()
                .unwrap()
                .description_url_escapes
                .is_empty()
        );
    }

    /// Oversized projections fall back to the complete raw field without a retained copy.
    #[test]
    fn archive_url_display_cache_bounds_do_not_truncate_metadata() {
        let mut cache = ArchiveDescriptionUrlCache::default();
        let mut details = DetailView {
            description: "https://e.t/%D0%AF".into(),
            ..DetailView::default()
        };
        cache.apply(&mut details);
        assert_eq!(cache.scans, 1);
        assert!(!details.description_url_escapes.is_empty());
        details.description = "x".repeat(crate::links::MAX_URL_DISPLAY_SOURCE_BYTES + 1);
        details.description.push_str(" https://e.t/%D0%AF END");
        let original = details.description.clone();
        cache.apply(&mut details);
        assert_eq!(details.description, original);
        assert!(details.description_url_escapes.is_empty());
        assert!(cache.source.is_empty());
        assert!(cache.escapes.is_empty());
        assert_eq!(cache.scans, 1);
    }

    /// Only the validated public profile URL supplies a search identity.
    #[test]
    fn archive_uploader_link_uses_profile_identity_not_its_display_label() {
        let mut value = item();
        let uploader = value.uploader.as_mut().unwrap();
        uploader.name = "Different label: private-looking@example.com".into();
        uploader.url = url::Url::parse("https://archive.org/details/@actual-account").unwrap();
        let details = detail_view(&value, None);
        let link = details
            .links
            .iter()
            .find(|link| link.prefix == "Uploader: ")
            .unwrap();
        assert_eq!(link.label, "Different label: private-looking@example.com");
        assert_eq!(
            link.internal_target,
            Some(DetailLinkInternalTarget::ArchiveUploader(
                "@actual-account".into()
            ))
        );
        assert_eq!(link.url, "https://archive.org/details/@actual-account");
        assert_eq!(
            details.channel_webpage_url.as_ref().map(url::Url::as_str),
            Some(link.url.as_str())
        );
        for invalid in [
            "https://archive.org/details/not-a-profile",
            "https://example.com/details/@pretend",
            "https://archive.org/details/@pretend?query=other",
        ] {
            value.uploader.as_mut().unwrap().name = "@pretend".into();
            value.uploader.as_mut().unwrap().url = url::Url::parse(invalid).unwrap();
            assert!(
                detail_view(&value, None)
                    .links
                    .iter()
                    .filter(|link| link.prefix == "Uploader: ")
                    .all(|link| link.internal_target.is_none())
            );
        }
        value.uploader = None;
        assert!(
            !detail_view(&value, None)
                .links
                .iter()
                .any(|link| link.prefix == "Uploader: ")
        );
    }

    /// Indexed activation works without an external opener and shares metadata Back.
    #[test]
    fn archive_uploader_navigation_uses_scope_and_history_without_external_opener() {
        let (_temporary, mut app) = lookup_controller();
        occupy_archive_worker(&mut app);
        app.view.screen = Screen::ArchiveOrg;
        app.view.external_opener_available = false;
        app.archive_org.items = vec![item()];
        app.archive_org.submitted_query = "old search".into();
        app.archive_org_search_query = "old search".into();
        app.populate_archive_org();
        let index = app
            .view
            .details
            .as_ref()
            .unwrap()
            .links
            .iter()
            .position(|link| link.prefix == "Uploader: ")
            .unwrap();
        assert_eq!(
            app.current_channel_url().as_deref(),
            Some("https://archive.org/details/@uploader")
        );
        app.dispatch(UiAction::OpenChannelInBrowser);
        assert!(app.view.status_line.contains("unavailable"));
        app.dispatch(UiAction::ActivateDetailLink(index));
        assert!(
            matches!(&app.archive_org.pending.as_ref().unwrap().kind, ArchiveRequest::Search(request) if request.scope == ArchiveOrgSearchScope::Uploader && request.query == "@uploader")
        );
        assert_eq!(app.archive_org.history.len(), 1);
        assert!(app.go_back_archive_org());
        assert_eq!(app.archive_org.submitted_query, "old search");
        assert_eq!(app.archive_org.submitted_scope, ArchiveOrgSearchScope::Text);
        app.dispatch(UiAction::SearchArchiveUploader("@uploader".into()));
        assert_eq!(
            app.archive_org.submitted_scope,
            ArchiveOrgSearchScope::Uploader
        );
        let generation = app.archive_org.generation;
        app.dispatch(UiAction::SearchArchiveUploader(
            "private-looking@example.com".into(),
        ));
        assert_eq!(
            app.archive_org.generation, generation,
            "invalid explicit identities must not navigate"
        );
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    /// Search tiles are withheld until metadata has selected the native waveform
    /// or confirmed that only the item tile is available.
    #[test]
    fn archive_waveform_preview_waits_for_metadata_before_requesting_item_tile() {
        let (_temporary, mut app) = lookup_controller();
        let (requests, _pending_requests) = bounded(8);
        app.provider_requests = Some(requests);
        app.view.screen = Screen::ArchiveOrg;
        let mut details = (*lookup_details("fixture")).clone();
        let tile = details.item.artwork_url.clone();
        app.archive_org.items = vec![details.item.clone()];
        app.populate_archive_org();
        let preview = app.view.details.as_ref().unwrap();
        assert_eq!(
            preview.thumbnail_url, None,
            "do not fetch the small search tile first"
        );
        assert_eq!(preview.expanded_thumbnail_url, None);

        let waveform = url::Url::parse(
            "https://iiif.archive.org/image/iiif/3/fixture%2Faudio.png/full/max/0/default.jpg",
        )
        .unwrap();
        details.item.artwork_url = Some(waveform.clone());
        app.archive_org
            .cache
            .push_back(("fixture".into(), Ok(Arc::new(details))));
        app.update_archive_org_detail();
        assert_eq!(
            app.view.details.as_ref().unwrap().thumbnail_url,
            Some(waveform)
        );

        app.archive_org.cache.clear();
        app.archive_org
            .cache
            .push_back(("fixture".into(), Err("metadata unavailable".into())));
        app.update_archive_org_detail();
        assert_eq!(app.view.details.as_ref().unwrap().thumbnail_url, tile);
    }

    /// Passive enrichment preserves the user's current artwork overlay for both
    /// catalogue containers and playable tracks, without reopening a closed one.
    #[test]
    fn archive_artwork_expansion_survives_same_identity_metadata_refresh() {
        for opened in [false, true] {
            let (_temporary, mut app) = lookup_controller();
            app.view.screen = Screen::ArchiveOrg;
            let original = lookup_details("fixture");
            app.archive_org.items = vec![original.item.clone()];
            app.archive_org
                .cache
                .push_back(("fixture".into(), Ok(Arc::clone(&original))));
            app.archive_org.active = opened.then(|| Arc::clone(&original));
            app.populate_archive_org();
            app.dispatch(UiAction::ToggleThumbnailExpansion);
            assert!(app.view.expanded_thumbnail_available());
            let mut enriched = (*original).clone();
            enriched.item.description = Some("Enriched description with more metadata".into());
            app.archive_org.cache.clear();
            app.archive_org
                .cache
                .push_back(("fixture".into(), Ok(Arc::new(enriched.clone()))));
            app.archive_org.active = opened.then(|| Arc::new(enriched));
            app.update_archive_org_detail();
            assert!(
                app.view.expanded_thumbnail_available(),
                "same identity, opened={opened}"
            );
            assert!(
                app.view
                    .details
                    .as_ref()
                    .unwrap()
                    .description
                    .contains("Enriched description")
            );
            app.dispatch(UiAction::ToggleThumbnailExpansion);
            app.update_archive_org_detail();
            assert!(
                !app.view.expanded_thumbnail_available(),
                "refresh must not reopen a closed overlay"
            );
        }
    }

    /// Expansion belongs to the exact item/file identity, never to a row index.
    #[test]
    fn archive_artwork_expansion_resets_on_new_item_track_or_missing_artwork() {
        let (_temporary, mut app) = lookup_controller();
        app.view.screen = Screen::ArchiveOrg;
        let first = lookup_details("first");
        let second = lookup_details("second");
        app.archive_org.items = vec![first.item.clone(), second.item.clone()];
        app.archive_org
            .cache
            .push_back(("first".into(), Ok(Arc::clone(&first))));
        app.archive_org
            .cache
            .push_back(("second".into(), Ok(Arc::clone(&second))));
        app.populate_archive_org();
        app.dispatch(UiAction::ToggleThumbnailExpansion);
        app.dispatch(UiAction::SelectRow(1));
        assert!(!app.view.expanded_thumbnail_available());

        let mut tracks = (*second).clone();
        let mut next = tracks.tracks[0].clone();
        next.filename = "02.opus".into();
        next.download_url = url::Url::parse("https://archive.org/download/second/02.opus").unwrap();
        tracks.tracks.push(next);
        app.archive_org.active = Some(Arc::new(tracks));
        app.archive_org_selected = 0;
        app.populate_archive_org();
        app.dispatch(UiAction::ToggleThumbnailExpansion);
        app.dispatch(UiAction::SelectRow(1));
        assert!(!app.view.expanded_thumbnail_available());

        app.dispatch(UiAction::ToggleThumbnailExpansion);
        let details = Arc::make_mut(app.archive_org.active.as_mut().unwrap());
        details.item.artwork_url = None;
        details.tracks[1].waveform_url = None;
        app.update_archive_org_detail();
        assert!(!app.view.details.as_ref().unwrap().thumbnail_expanded);
    }

    pub(super) fn item() -> ArchiveOrgItem {
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
            download_variants: Vec::new(),
            filename: "01 chapter.opus".into(),
            title: "First chapter".into(),
            download_url: url::Url::parse("https://archive.org/download/fixture/01%20chapter.opus")
                .unwrap(),
            duration_seconds: Some(123),
            size_bytes: Some(1024),
        }
    }

    /// A standalone controller keeps lookup tests offline and independent of selected rows.
    pub(super) fn lookup_controller() -> (tempfile::TempDir, AppController) {
        let directory = crate::test_support::canonical_tempdir("archive download lookup");
        let config = Config::for_dir(directory.path().join("config"));
        let store = StateStore::open_in_memory().unwrap();
        let mut controller = AppController::new(config, store, None, None);
        controller.view.screen = Screen::History;
        controller.archive_org.initialized = true;
        (directory, controller)
    }

    /// Blocks real provider work while a test inspects queued request ownership.
    pub(super) fn occupy_archive_worker(controller: &mut AppController) {
        let (_sender, response) = bounded(1);
        controller.archive_org.worker = Some(ArchiveWorker {
            job: ArchiveJob {
                generation: 0,
                kind: ArchiveRequest::Details {
                    identifier: "fixture".into(),
                    open: false,
                },
                due: Instant::now(),
            },
            response,
            thread: thread::spawn(|| {}),
        });
    }

    #[test]
    fn archive_metadata_links_keep_original_creator_values_and_topic_byte_ranges() {
        let mut value = item();
        value.creators = vec!["Бьорк; orchestra".into(), "Another artist".into()];
        value.creator = Some(value.creators.join("; "));
        value.topics = vec!["Live music".into(), "История".into()];
        let details = detail_view(&value, None);
        assert!(details.description.starts_with(
            "Creator: Бьорк; orchestra; Another artist\nTopics: Live music, История\n"
        ));
        let links: Vec<_> = details
            .links
            .iter()
            .filter(|link| link.description_range.is_some())
            .collect();
        assert_eq!(links.len(), 4);
        for (link, expected) in links.iter().zip([
            "Бьорк; orchestra",
            "Another artist",
            "Live music",
            "История",
        ]) {
            let range = link.description_range.unwrap();
            assert_eq!(
                &details.description[range.start_byte..range.end_byte],
                expected
            );
            assert_eq!(link.label, expected);
            assert!(
                link.url.is_empty(),
                "metadata must navigate internally, not open a browser"
            );
        }
        assert_eq!(
            links[0].internal_target.as_ref().unwrap().action(),
            UiAction::SearchArchiveCreator("Бьорк; orchestra".into())
        );
        assert_eq!(
            links[3].internal_target.as_ref().unwrap().action(),
            UiAction::SearchArchiveTopic("История".into())
        );
        assert!(details.description.ends_with("Original description"));
    }

    #[test]
    fn archive_metadata_search_replaces_old_query_and_survives_draft_restart_and_pagination() {
        use crate::domain::ArchiveOrgSearchScope;
        let (_directory, mut controller) = lookup_controller();
        controller.view.screen = Screen::ArchiveOrg;
        controller.archive_org_search_query = "old unrelated search".into();
        occupy_archive_worker(&mut controller);
        controller.dispatch(UiAction::SearchArchiveCreator("Бьорк; orchestra".into()));
        assert_eq!(
            controller.archive_org_search_scope,
            ArchiveOrgSearchScope::Creator
        );
        let request = controller.archive_org.pending.as_ref().unwrap();
        assert!(
            matches!(&request.kind, ArchiveRequest::Search(request) if request.scope == ArchiveOrgSearchScope::Creator && request.query == "Бьорк; orchestra")
        );
        controller.archive_org.pending = None;
        controller.archive_org.request = None;
        controller.view.search_editing = true;
        controller.view.search_query = "not submitted".into();
        assert!(controller.save_session());
        let saved = controller.store.session().unwrap().unwrap();
        assert_eq!(
            saved.archive_org_search_scope,
            ArchiveOrgSearchScope::Creator
        );
        assert_eq!(saved.archive_org_search_text, "Бьорк; orchestra");
        let restart_store = StateStore::open_in_memory().unwrap();
        let mut restart_session = saved;
        restart_session.screen = StoredScreen::History;
        restart_store.save_session(&restart_session, 1).unwrap();
        let mut restarted =
            AppController::new(controller.config.clone(), restart_store, None, None);
        occupy_archive_worker(&mut restarted);
        restarted.show_screen(Screen::ArchiveOrg);
        assert!(
            matches!(&restarted.archive_org.pending.as_ref().unwrap().kind, ArchiveRequest::Search(request) if request.scope == ArchiveOrgSearchScope::Creator && request.query == "Бьорк; orchestra")
        );
        restarted
            .archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
        controller.show_screen(Screen::History);
        controller.show_screen(Screen::ArchiveOrg);
        assert_eq!(controller.view.search_query, "Бьорк; orchestra");
        controller.archive_org.next_page = Some(2);
        controller.activate_archive_org_selection();
        assert!(
            matches!(&controller.archive_org.pending.as_ref().unwrap().kind, ArchiveRequest::Search(request) if request.page == 2 && request.scope == ArchiveOrgSearchScope::Creator && request.query == "Бьорк; orchestra")
        );
        controller.archive_org.initialized = false;
        controller.populate_archive_org();
        assert!(
            matches!(&controller.archive_org.pending.as_ref().unwrap().kind, ArchiveRequest::Search(request) if request.page == 1 && request.scope == ArchiveOrgSearchScope::Creator)
        );
        controller.dispatch(UiAction::SearchArchiveTopic("Live music".into()));
        assert_eq!(
            controller.archive_org_search_scope,
            ArchiveOrgSearchScope::Topic
        );
        controller.submit_archive_org_search("new plain search".into());
        assert_eq!(
            controller.archive_org_search_scope,
            ArchiveOrgSearchScope::Text
        );
        assert!(
            matches!(&controller.archive_org.pending.as_ref().unwrap().kind, ArchiveRequest::Search(request) if request.scope == ArchiveOrgSearchScope::Text && request.query == "new plain search")
        );
        controller
            .archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_metadata_inline_keyboard_reveal_stops_after_manual_scroll() {
        let (_directory, mut controller) = lookup_controller();
        controller.view.details = Some(DetailView {
            expanded_wikidata_item: Some("Q42".into()),
            links: vec![DetailLinkView {
                description_range: Some(crate::view::DetailHighlightRange {
                    start_byte: 0,
                    end_byte: 3,
                }),
                ..DetailLinkView::default()
            }],
            ..DetailView::default()
        });
        controller.dispatch(UiAction::SelectDetailLink(0));
        assert_eq!(controller.view.detail_link_reveal, Some(0));
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .expanded_wikidata_item
                .is_none(),
            "keyboard metadata focus must restore its visible description"
        );
        controller.dispatch(UiAction::SetDetailsScroll(20));
        assert_eq!(controller.view.detail_link_reveal, None);
        controller.dispatch(UiAction::MoveDetailLink(1));
        assert_eq!(controller.view.detail_link_reveal, Some(0));
        controller.dispatch(UiAction::ScrollDetails(DetailsScroll::Home));
        assert_eq!(controller.view.detail_link_reveal, None);
    }

    #[test]
    fn archive_metadata_indexed_activation_does_not_require_an_external_opener() {
        let (_directory, mut controller) = lookup_controller();
        controller.view.external_opener_available = false;
        let details = detail_view(&item(), None);
        let index = details
            .links
            .iter()
            .position(|link| {
                matches!(
                    link.internal_target,
                    Some(DetailLinkInternalTarget::ArchiveTopic(_))
                )
            })
            .unwrap();
        controller.view.details = Some(details);
        occupy_archive_worker(&mut controller);
        controller.dispatch(UiAction::ActivateDetailLink(index));
        assert!(
            matches!(&controller.archive_org.pending.as_ref().unwrap().kind, ArchiveRequest::Search(request) if request.scope == ArchiveOrgSearchScope::Topic && request.query == "History")
        );
        controller
            .archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_search_highlighting_uses_submitted_query_after_draft_save_and_tab_switch() {
        let (_directory, mut controller) = lookup_controller();
        controller.view.screen = Screen::ArchiveOrg;
        // A completed synthetic worker occupies the slot until polled, keeping
        // submission offline while exercising the real submitted-query path.
        let occupy_worker = |controller: &mut AppController| {
            let (_sender, response) = bounded(1);
            controller.archive_org.worker = Some(ArchiveWorker {
                job: ArchiveJob {
                    generation: 0,
                    kind: ArchiveRequest::Details {
                        identifier: "fixture".into(),
                        open: false,
                    },
                    due: Instant::now(),
                },
                response,
                thread: thread::spawn(|| {}),
            });
        };
        occupy_worker(&mut controller);
        controller.submit_archive_org_search("original".to_owned());
        controller
            .archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
        // Install the completed search's offline metadata without starting HTTP.
        controller.archive_org.pending = None;
        controller.archive_org.request = None;
        controller.archive_org.active = Some(lookup_details("fixture"));
        controller.populate_archive_org();
        let highlights = controller
            .view
            .details
            .as_ref()
            .unwrap()
            .search_highlights
            .clone();
        assert!(!highlights.is_empty());

        controller.view.search_editing = true;
        controller.view.search_query = "unsubmitted draft".to_owned();
        assert!(controller.save_session());
        controller.update_archive_org_detail();
        assert_eq!(
            controller.view.details.as_ref().unwrap().search_highlights,
            highlights
        );

        controller.show_screen(Screen::History);
        controller.show_screen(Screen::ArchiveOrg);
        assert_eq!(
            controller.view.details.as_ref().unwrap().search_highlights,
            highlights
        );
        assert!(
            controller.archive_org.worker.is_none(),
            "draft restoration must not start a new search"
        );
        controller.archive_org.active = None;
        controller.archive_org.next_page = Some(2);
        occupy_worker(&mut controller);
        controller.activate_archive_org_selection();
        let ArchiveRequest::Search(request) =
            &controller.archive_org.pending.as_ref().unwrap().kind
        else {
            panic!("expected the next search page");
        };
        assert_eq!(
            request.query, "original",
            "continuation retains the displayed results' query"
        );
        assert_eq!(request.page, 2);
        controller
            .archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    pub(super) fn lookup_details(identifier: &str) -> Arc<ArchiveOrgItemDetails> {
        let mut value = item();
        value.identifier = identifier.to_owned();
        value.webpage_url =
            url::Url::parse(&format!("https://archive.org/details/{identifier}")).unwrap();
        let mut audio = track();
        audio.download_url = url::Url::parse(&format!(
            "https://archive.org/download/{identifier}/01%20chapter.opus"
        ))
        .unwrap();
        audio.download_variants = serde_json::from_value(serde_json::json!([
            {"filename": "original.flac", "download_url": format!("https://archive.org/download/{identifier}/original.flac"),
             "format": "Flac", "size_bytes": 9000, "provenance": "original", "is_video": false},
            {"filename": "01 chapter.opus", "download_url": audio.download_url,
             "format": "Ogg Opus", "size_bytes": 1000, "provenance": "derivative", "is_video": false}
        ])).unwrap();
        Arc::new(ArchiveOrgItemDetails {
            item: value,
            tracks: vec![audio],
            comments: Vec::new(),
        })
    }

    /// A gated metadata transport proves asynchronous ownership without accessing a real item.
    struct LookupTransport {
        entered: Sender<url::Url>,
        release: Receiver<Result<Vec<u8>, crate::providers::ProviderError>>,
        count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::providers::archive_org::ArchiveOrgTransport for LookupTransport {
        fn fetch(
            &self,
            url: &url::Url,
            _: usize,
        ) -> Result<Vec<u8>, crate::providers::ProviderError> {
            if !url.path().starts_with("/metadata/") {
                return Err(crate::providers::ProviderError::HttpStatus(404));
            }
            self.count.fetch_add(1, AtomicOrdering::SeqCst);
            self.entered
                .send(url.clone())
                .expect("test still receiving");
            self.release
                .recv_timeout(Duration::from_secs(5))
                .expect("test releases its bounded metadata worker")
        }
    }

    fn lookup_transport(
        controller: &mut AppController,
    ) -> (
        Receiver<url::Url>,
        Sender<Result<Vec<u8>, crate::providers::ProviderError>>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let (entered, requests) = bounded(1);
        let (release, responses) = bounded(1);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        controller.archive_org.client =
            ArchiveOrgClient::with_transport(Arc::new(LookupTransport {
                entered,
                release: responses,
                count: Arc::clone(&count),
            }));
        (requests, release, count)
    }

    fn lookup_metadata(identifier: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "metadata": {"identifier": identifier, "title": "Download inventory", "mediatype": "audio"},
            "files": [
                {"name": "original.flac", "source": "original", "format": "Flac"},
                {"name": "01 chapter.opus", "source": "derivative", "original": "original.flac", "format": "Ogg Opus"}
            ]
        })).unwrap()
    }

    fn finish_lookup_worker(controller: &mut AppController) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.archive_org.worker.is_some() {
            assert!(
                Instant::now() < deadline,
                "bounded metadata worker completed"
            );
            controller.poll_archive_org_worker();
            thread::yield_now();
        }
    }

    #[test]
    fn download_lookup_uses_active_or_cached_exact_family_without_navigation() {
        for active in [true, false] {
            let (_directory, mut controller) = lookup_controller();
            let details = lookup_details("fixture");
            let source = details.tracks[0].download_variants[0].download_url.clone();
            if active {
                controller.archive_org.active = Some(Arc::clone(&details));
            } else {
                controller
                    .archive_org
                    .cache
                    .push_back(("fixture".into(), Ok(Arc::clone(&details))));
            }
            controller.view.selected = 7;
            controller.view.status_line = "Keep history".to_owned();
            let variants = controller
                .archive_download_variants(&source)
                .unwrap()
                .expect("cached inventory");
            assert_eq!(variants, details.tracks[0].download_variants);
            assert_eq!(controller.view.screen, Screen::History);
            assert_eq!(controller.view.selected, 7);
            assert_eq!(controller.view.status_line, "Keep history");
            assert!(controller.archive_org.worker.is_none());
        }
    }

    #[test]
    fn download_lookup_rejects_untrusted_or_noncanonical_sources_without_requests() {
        let (_directory, mut controller) = lookup_controller();
        for source in [
            "https://example.org/download/fixture/audio.mp3",
            "http://archive.org/download/fixture/audio.mp3",
            "https://user:secret@archive.org/download/fixture/audio.mp3",
            "https://archive.org:8443/download/fixture/audio.mp3",
            "https://archive.org/details/fixture",
            "https://archive.org/download/fixture/audio.mp3?token=secret",
            "https://archive.org/download/fixture/audio.mp3#part",
            "https://archive.org/download/bad%20id/audio.mp3",
            "https://archive.org/download/fixture/%2Fescape.mp3",
            "https://archive.org/download/fixture/%5Cescape.mp3",
            "https://archive.org/download/fixture/%00audio.mp3",
            "https://archive.org/download/fixture/%FFaudio.mp3",
            "https://archive.org/download/fixture/bad%escape.mp3",
            "https://archive.org/download/fixture/empty//audio.mp3",
        ] {
            let source = url::Url::parse(source).unwrap();
            assert!(
                controller.archive_download_variants(&source).is_err(),
                "{source}"
            );
            assert!(controller.archive_org.pending.is_none());
            assert!(controller.archive_org.worker.is_none());
        }
    }

    #[test]
    fn download_lookup_refreshes_old_inventory_and_survives_passive_other_selection() {
        let (_directory, mut controller) = lookup_controller();
        let mut old = (*lookup_details("fixture")).clone();
        let source = old.tracks[0].download_url.clone();
        old.tracks[0].download_variants.clear();
        controller
            .archive_org
            .cache
            .push_back(("fixture".into(), Ok(Arc::new(old))));
        let other = lookup_details("other");
        controller.archive_org.items.push(other.item.clone());
        controller
            .archive_org
            .cache
            .push_back(("other".into(), Ok(other)));
        let (requests, release, count) = lookup_transport(&mut controller);
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            requests
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .path(),
            "/metadata/fixture"
        );
        let generation = controller.archive_org.pending.as_ref().unwrap().generation;
        controller.view.screen = Screen::ArchiveOrg;
        controller.update_archive_org_detail();
        assert_eq!(
            controller.archive_org.pending.as_ref().unwrap().generation,
            generation
        );
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_none()
        );
        release.send(Ok(lookup_metadata("fixture"))).unwrap();
        finish_lookup_worker(&mut controller);
        let variants = controller
            .archive_download_variants(&source)
            .unwrap()
            .expect("complete inventory");
        assert_eq!(variants.len(), 2);
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(controller.view.screen, Screen::ArchiveOrg);
        assert!(controller.archive_org.active.is_none());
        assert_eq!(
            controller.view.details.as_ref().unwrap().title,
            "An audio collection"
        );
    }

    #[test]
    fn download_lookup_cancellation_keeps_late_results_passive() {
        let (_directory, mut controller) = lookup_controller();
        let source = lookup_details("fixture").tracks[0].download_url.clone();
        let (requests, release, _) = lookup_transport(&mut controller);
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_none()
        );
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        controller.cancel_archive_download_lookup();
        assert!(controller.archive_org.pending.is_none());
        release.send(Ok(lookup_metadata("fixture"))).unwrap();
        finish_lookup_worker(&mut controller);
        assert!(controller.cached_archive_details("fixture").is_some());
        assert!(controller.archive_org.active.is_none());
        assert_eq!(controller.view.screen, Screen::History);
        assert!(controller.archive_org.pending.is_none());
    }

    #[test]
    fn download_lookup_errors_are_terminal_until_explicit_cancellation() {
        let (_directory, mut controller) = lookup_controller();
        let source = lookup_details("fixture").tracks[0].download_url.clone();
        let (requests, release, count) = lookup_transport(&mut controller);
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_none()
        );
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        release
            .send(Err(crate::providers::ProviderError::HttpStatus(503)))
            .unwrap();
        finish_lookup_worker(&mut controller);
        for _ in 0..3 {
            assert!(controller.archive_download_variants(&source).is_err());
        }
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
        assert!(controller.archive_org.worker.is_none());
        assert_eq!(controller.view.screen, Screen::History);
    }

    #[test]
    fn download_lookup_reuses_pending_details_without_leaving_open_activity() {
        let (_directory, mut controller) = lookup_controller();
        controller.view.screen = Screen::ArchiveOrg;
        let source = lookup_details("fixture").tracks[0].download_url.clone();
        let (requests, release, count) = lookup_transport(&mut controller);
        let job = ArchiveJob {
            generation: 7,
            kind: ArchiveRequest::Details {
                identifier: "fixture".into(),
                open: true,
            },
            due: Instant::now() + Duration::from_secs(60),
        };
        controller.archive_org.generation = 7;
        controller.archive_org.pending = Some(job.clone());
        controller.archive_org.request = Some(job);
        controller.begin_search_activity(SearchActivity::ArchiveOrg);
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            controller.archive_org.pending.as_ref().unwrap().generation,
            7
        );
        assert_eq!(controller.view.search_activity, None);
        controller.poll_archive_org_worker();
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(Ok(lookup_metadata("fixture"))).unwrap();
        finish_lookup_worker(&mut controller);
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_some()
        );
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
        assert!(controller.archive_org.active.is_none());
    }

    #[test]
    fn download_lookup_cancel_does_not_revoke_a_newer_navigation_owner() {
        let (_directory, mut controller) = lookup_controller();
        let source = lookup_details("fixture").tracks[0].download_url.clone();
        let (requests, release, _) = lookup_transport(&mut controller);
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_none()
        );
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        let next = ArchiveJob {
            generation: controller.archive_org.generation + 1,
            kind: ArchiveRequest::Details {
                identifier: "other".into(),
                open: true,
            },
            due: Instant::now() + Duration::from_secs(60),
        };
        controller.archive_org.generation = next.generation;
        controller.archive_org.pending = Some(next.clone());
        controller.archive_org.request = Some(next.clone());
        controller.cancel_archive_download_lookup();
        assert_eq!(
            controller.archive_org.pending.as_ref().unwrap().generation,
            next.generation
        );
        release.send(Ok(lookup_metadata("fixture"))).unwrap();
        finish_lookup_worker(&mut controller);
        assert_eq!(
            controller.archive_org.pending.as_ref().unwrap().generation,
            next.generation
        );
        assert!(controller.archive_org.active.is_none());
    }

    #[test]
    fn download_lookup_missing_exact_file_is_terminal_without_a_substitute() {
        let (_directory, mut controller) = lookup_controller();
        let source = url::Url::parse("https://archive.org/download/fixture/missing.mp3").unwrap();
        let (requests, release, count) = lookup_transport(&mut controller);
        assert!(
            controller
                .archive_download_variants(&source)
                .unwrap()
                .is_none()
        );
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(Ok(lookup_metadata("fixture"))).unwrap();
        finish_lookup_worker(&mut controller);
        for _ in 0..3 {
            assert!(
                controller
                    .archive_download_variants(&source)
                    .unwrap_err()
                    .contains("no available download variants")
            );
        }
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
        assert!(controller.archive_org.worker.is_none());
    }

    #[test]
    fn download_lookup_canonical_source_supports_unicode_literal_percent_and_nested_files() {
        let mut source = url::Url::parse("https://archive.org/").unwrap();
        source.path_segments_mut().unwrap().extend([
            "download",
            "fixture",
            "Звук",
            "01 ?#%+ chapter.opus",
        ]);
        assert_eq!(
            archive_download_identifier(&source).as_deref(),
            Some("fixture")
        );
        let too_long = url::Url::parse(&format!(
            "https://archive.org/download/fixture/{}.mp3",
            "a".repeat(2049)
        ))
        .unwrap();
        assert_eq!(archive_download_identifier(&too_long), None);
    }

    /// A real selected Archive track enters the chooser without starting a helper.
    #[cfg(feature = "yt-dlp")]
    #[test]
    fn selected_archive_download_keeps_snapshot_and_dismisses_without_a_child() {
        struct NeverStart;
        impl DownloadLauncher for NeverStart {
            fn start(&mut self, _: &DownloadRequest) -> Result<Box<dyn RunningDownload>, String> {
                panic!("showing/dismissing download choices must not launch a process");
            }
        }
        let (_directory, mut controller) = lookup_controller();
        controller.download_launcher = Box::new(NeverStart);
        controller.config.downloads.archive_format =
            crate::config::ArchiveDownloadPreference::AskEachTime;
        let mut details = (*lookup_details("fixture")).clone();
        let selected = &mut details.tracks[0];
        selected.filename = "generated.mp3".into();
        selected.download_url =
            url::Url::parse("https://archive.org/download/fixture/generated.mp3").unwrap();
        selected.download_variants[1].filename = selected.filename.clone();
        selected.download_variants[1].format = "VBR MP3".into();
        selected.download_variants[1].download_url = selected.download_url.clone();
        let mut other = selected.clone();
        other.title = "Other selected row".into();
        other.filename = "other.mp3".into();
        other.download_url =
            url::Url::parse("https://archive.org/download/fixture/other.mp3").unwrap();
        other.download_variants.clear();
        details.tracks.push(other);
        controller.archive_org.active = Some(Arc::new(details));
        controller.view.screen = Screen::ArchiveOrg;
        controller.view.selected = 0;
        controller.start_selected_download();
        let popup = controller
            .view
            .download_choice_popup
            .as_ref()
            .expect("format chooser");
        assert_eq!(popup.options.len(), 2);
        assert!(popup.options[0].contains("Original: Flac"));
        assert!(popup.options[0].contains("original.flac"));
        assert!(popup.options[1].contains("Archive-generated: VBR MP3"));
        assert!(popup.options[1].contains("generated.mp3"));
        let options = popup.options.clone();
        controller.view.selected = 1;
        controller.poll_manual_download_choice();
        assert_eq!(
            controller
                .view
                .download_choice_popup
                .as_ref()
                .unwrap()
                .options,
            options
        );
        assert!(controller.active_download.is_none());
        controller.dismiss_download_choice();
        assert!(controller.view.download_choice_popup.is_none());
        assert!(controller.pending_download_choice.is_none());
        assert!(controller.active_download.is_none());
        assert!(controller.archive_org.worker.is_none());
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
