//! Bounded, move-only navigation between submitted Archive metadata searches.

use super::*;

const MAX_LOCATIONS: usize = 16;
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

/// Logical navigation survives cache eviction without retaining large descriptions.
pub(super) struct ArchiveLocation {
    query: String,
    scope: ArchiveOrgSearchScope,
    selected: usize,
    selected_id: Option<String>,
    active_id: Option<String>,
    track: usize,
    filename: Option<String>,
    scroll: usize,
    focused: bool,
    cached: Option<CachedLocation>,
}

/// Large catalogue strings move into history; item metadata retains one shared Arc.
struct CachedLocation {
    items: Vec<ArchiveOrgItem>,
    active: Option<Arc<ArchiveOrgItemDetails>>,
    next_page: Option<u32>,
    total: u64,
    weight: usize,
}

impl AppController {
    /// Captures only Archive navigation; playback and manual downloads are untouched.
    pub(super) fn remember_archive_location(&mut self) {
        // A second hop during restoration remembers the logical destination,
        // never a half-loaded page under the destination's title.
        let location = if let Some(location) = self.archive_org.restoring.take() {
            location
        } else {
            let selected = if self.archive_org.active.is_some() {
                self.archive_org.search_selected
            } else {
                self.view.selected
            };
            let mut location = ArchiveLocation {
                query: self.archive_org.submitted_query.clone(),
                scope: self.archive_org.submitted_scope,
                selected,
                selected_id: self
                    .archive_org
                    .items
                    .get(selected)
                    .map(|item| item.identifier.clone()),
                active_id: self
                    .archive_org
                    .active
                    .as_ref()
                    .map(|details| details.item.identifier.clone()),
                track: self.view.selected,
                filename: self
                    .archive_org
                    .active
                    .as_ref()
                    .and_then(|details| details.tracks.get(self.view.selected))
                    .map(|track| track.filename.clone()),
                scroll: self.view.details_scroll,
                focused: self.view.details_focused,
                cached: None,
            };
            let items = std::mem::take(&mut self.archive_org.items);
            let active = self.archive_org.active.take();
            // A pending search is not a completed empty/partial catalogue.
            // Preserve its logical query, then refetch if the user returns.
            if !self
                .archive_org
                .pending
                .as_ref()
                .is_some_and(|job| matches!(job.kind, ArchiveRequest::Search(_)))
                && let Some(weight) = retained_weight(&items, items.capacity(), active.as_deref())
            {
                location.cached = Some(CachedLocation {
                    items,
                    active,
                    weight,
                    next_page: self.archive_org.next_page,
                    total: self.archive_org.total,
                });
            }
            location
        };
        self.archive_org.history.push_back(location);
        while self.archive_org.history.len() > MAX_LOCATIONS {
            self.archive_org.history.pop_front();
        }
        let mut retained = self
            .archive_org
            .history
            .iter()
            .filter_map(|route| route.cached.as_ref())
            .map(|cached| cached.weight)
            .sum::<usize>();
        for route in &mut self.archive_org.history {
            if retained <= MAX_RETAINED_BYTES {
                break;
            }
            if let Some(cached) = route.cached.take() {
                retained -= cached.weight;
            }
        }
    }

    /// Returns to the newest logical location, reloading evicted data when needed.
    pub(super) fn restore_archive_location(&mut self) -> bool {
        let Some(mut location) = self.archive_org.history.pop_back() else {
            return false;
        };
        self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
        self.archive_org.pending = None;
        self.archive_org.request = None;
        self.archive_org.restoring = None;
        self.view.search_editing = false;
        self.view.selected_detail_link = None;
        self.view.detail_link_reveal = None;
        self.view.details_text_selection = None;
        if let Some(cached) = location.cached.take() {
            self.archive_org_search_query.clone_from(&location.query);
            self.archive_org.submitted_query.clone_from(&location.query);
            self.archive_org_search_scope = location.scope;
            self.archive_org.submitted_scope = location.scope;
            self.archive_org.items = cached.items;
            self.archive_org.active = cached.active;
            self.archive_org.next_page = cached.next_page;
            self.archive_org.total = cached.total;
        } else {
            self.start_archive_org_search(location.query.clone(), location.scope);
        }
        self.archive_org.restoring = Some(location);
        if !self.continue_archive_restore() {
            self.populate_archive_org();
        }
        true
    }

    /// Completes bounded page/item restoration only for the currently owned response.
    pub(super) fn continue_archive_restore(&mut self) -> bool {
        if self.archive_org.pending.is_some() || self.view.screen != Screen::ArchiveOrg {
            return false;
        }
        let Some(location) = self.archive_org.restoring.as_ref() else {
            return false;
        };
        let selected = location.selected_id.as_ref().and_then(|id| {
            self.archive_org
                .items
                .iter()
                .position(|item| item.identifier == *id)
        });
        if selected.is_none()
            && (location.selected_id.is_some() || self.archive_org.items.len() <= location.selected)
            && let Some(page) = self.archive_org.next_page.filter(|page| *page <= 20)
        {
            self.queue_archive_request(
                ArchiveRequest::Search(ArchiveOrgSearchRequest {
                    query: location.query.clone(),
                    scope: location.scope,
                    page,
                    limit: 50,
                }),
                false,
            );
            return false;
        }
        self.archive_org.search_selected = selected
            .unwrap_or(location.selected)
            .min(self.archive_org.items.len().saturating_sub(1));
        self.archive_org_selected = self.archive_org.search_selected;
        if let Some(identifier) = location.active_id.clone()
            && self
                .archive_org
                .active
                .as_ref()
                .is_none_or(|details| details.item.identifier != identifier)
        {
            match self.cached_archive_details(&identifier) {
                Some(Ok(details)) => self.archive_org.active = Some(details),
                Some(Err(_)) => {} // Failed metadata does not create an automatic retry loop.
                None => {
                    self.queue_archive_request(
                        ArchiveRequest::Details {
                            identifier,
                            open: true,
                        },
                        false,
                    );
                    return false;
                }
            }
        }
        let location = self
            .archive_org
            .restoring
            .take()
            .expect("owned Archive restoration");
        if let Some(details) = &self.archive_org.active {
            self.archive_org_selected = location
                .filename
                .as_ref()
                .and_then(|filename| {
                    details
                        .tracks
                        .iter()
                        .position(|track| track.filename == *filename)
                })
                .unwrap_or(location.track)
                .min(details.tracks.len().saturating_sub(1));
        }
        self.archive_org.message = if let Some(details) = &self.archive_org.active {
            format!(
                "{} audio tracks · Enter: play · d: download · Esc: back",
                details.tracks.len()
            )
        } else {
            format!(
                "{} of {} archive.org items · Enter: open · /: search",
                self.archive_org.items.len(),
                self.archive_org.total
            )
        };
        self.populate_archive_org();
        self.view.details_scroll = location.scroll;
        self.view.details_focused = location.focused;
        true
    }

    /// Publishes only currently actionable Back controls to both renderers.
    pub(super) fn update_archive_back_available(&mut self) {
        self.view.archive_org_back_available = !self.archive_download_lookup_pending()
            && (self.archive_org.active.is_some()
                || !self.archive_org.history.is_empty()
                || self.archive_org.pending.as_ref().is_some_and(|job| {
                    matches!(job.kind, ArchiveRequest::Details { open: true, .. })
                }));
    }

    /// A user's row movement supersedes automatic restoration without touching downloads.
    pub(in crate::app) fn cancel_archive_restore_for_selection(&mut self) {
        if self.view.screen == Screen::ArchiveOrg
            && self.archive_org.restoring.take().is_some()
            && !self.archive_download_lookup_pending()
        {
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
        }
    }
}

/// Allocation-free capacity accounting, including truncated text and spare rows.
/// A fourfold safety margin covers allocation overhead and URL's private backing
/// storage. Traversal stops on overflow; provider limits bound element counts.
fn retained_weight(
    items: &[ArchiveOrgItem],
    capacity: usize,
    active: Option<&ArchiveOrgItemDetails>,
) -> Option<usize> {
    let mut budget = RetainedBytes(0);
    budget.add(capacity.saturating_mul(std::mem::size_of::<ArchiveOrgItem>()))?;
    for item in items {
        budget.item(item)?;
    }
    if let Some(details) = active {
        budget.add(std::mem::size_of::<ArchiveOrgItemDetails>())?;
        budget.item(&details.item)?;
        budget.vector(&details.tracks)?;
        for track in &details.tracks {
            budget.add(track.filename.capacity())?;
            budget.add(track.title.capacity())?;
            budget.url(&track.download_url)?;
            if let Some(url) = &track.waveform_url {
                budget.url(url)?;
            }
            budget.vector(&track.download_variants)?;
            for variant in &track.download_variants {
                budget.add(variant.filename.capacity())?;
                budget.add(variant.format.capacity())?;
                budget.url(&variant.download_url)?;
            }
        }
        budget.vector(&details.comments)?;
        for comment in &details.comments {
            budget.add(comment.comment_id.capacity())?;
            budget.add(comment.author_name.capacity())?;
            budget.add(comment.text.capacity())?;
            if let Some(url) = &comment.author_channel_url {
                budget.url(url)?;
            }
        }
    }
    Some(budget.0 * 4)
}

/// Counts retained DTO allocation capacities; methods fail before exceeding the cap.
struct RetainedBytes(usize);

impl RetainedBytes {
    fn add(&mut self, bytes: usize) -> Option<()> {
        self.0 = self.0.checked_add(bytes)?;
        (self.0 <= MAX_RETAINED_BYTES / 4).then_some(())
    }

    // Vec rather than slice is intentional: cleared vectors still own capacity.
    fn vector<T>(&mut self, values: &Vec<T>) -> Option<()> {
        self.add(values.capacity().checked_mul(std::mem::size_of::<T>())?)
    }

    fn url(&mut self, url: &url::Url) -> Option<()> {
        self.add(url.as_str().len())
    }

    fn item(&mut self, item: &ArchiveOrgItem) -> Option<()> {
        self.add(item.identifier.capacity())?;
        self.add(item.title.capacity())?;
        for text in [&item.creator, &item.description, &item.license]
            .into_iter()
            .flatten()
        {
            self.add(text.capacity())?;
        }
        for values in [&item.creators, &item.topics, &item.languages] {
            self.vector(values)?;
            for value in values {
                self.add(value.capacity())?;
            }
        }
        self.vector(&item.collections)?;
        for link in item.collections.iter().chain(item.uploader.iter()) {
            self.add(link.name.capacity())?;
            self.url(&link.url)?;
        }
        self.url(&item.webpage_url)?;
        for url in [&item.artwork_url, &item.license_url].into_iter().flatten() {
            self.url(url)?;
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_back_pending_search_is_reloaded_instead_of_cached_as_empty() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.submit_archive_org_search("still loading".into());
        app.search_archive_metadata("topic".into(), ArchiveOrgSearchScope::Topic);
        assert!(app.go_back_archive_org());
        assert!(
            matches!(app.archive_org.pending.as_ref().map(|job| &job.kind), Some(ArchiveRequest::Search(request)) if request.query == "still loading")
        );
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_back_evicted_identity_can_move_beyond_its_old_page() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.archive_org.items = vec![super::super::tests::item()];
        app.search_archive_metadata("topic".into(), ArchiveOrgSearchScope::Topic);
        app.archive_org.history[0].cached = None;
        assert!(app.go_back_archive_org());
        let first = app.archive_org.pending.clone().unwrap();
        let mut other = super::super::tests::item();
        other.identifier = "newer".into();
        app.handle_archive_response(
            first,
            Ok(ArchiveResponse::Search(ArchiveOrgSearchPage {
                items: vec![other],
                page: 1,
                total: 2,
                next_page: Some(2),
            })),
        );
        let second = app
            .archive_org
            .pending
            .clone()
            .expect("exact old identity on a later page");
        assert!(matches!(&second.kind, ArchiveRequest::Search(request) if request.page == 2));
        app.handle_archive_response(
            second,
            Ok(ArchiveResponse::Search(ArchiveOrgSearchPage {
                items: vec![super::super::tests::item()],
                page: 2,
                total: 2,
                next_page: None,
            })),
        );
        assert_eq!(app.view.selected, 1);
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_back_budget_counts_spare_string_capacity() {
        let mut item = super::super::tests::item();
        let mut text = String::with_capacity(MAX_RETAINED_BYTES);
        text.push('x');
        item.description = Some(text);
        assert!(retained_weight(&[item], 1, None).is_none());
    }

    fn controller() -> (tempfile::TempDir, AppController) {
        let (temporary, mut app) = super::super::tests::lookup_controller();
        app.view.screen = Screen::ArchiveOrg;
        (temporary, app)
    }

    #[test]
    fn archive_back_evicted_track_reloads_metadata_and_defers_projection_off_tab() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.archive_org.items = vec![super::super::tests::item()];
        app.archive_org.active = Some(super::super::tests::lookup_details("fixture"));
        app.view.details_scroll = 12;
        app.search_archive_metadata("topic".into(), ArchiveOrgSearchScope::Topic);
        app.archive_org.history[0].cached = None;
        app.go_back_archive_org();
        let search = app.archive_org.pending.clone().unwrap();
        app.handle_archive_response(
            search,
            Ok(ArchiveResponse::Search(ArchiveOrgSearchPage {
                items: vec![super::super::tests::item()],
                page: 1,
                total: 1,
                next_page: None,
            })),
        );
        let metadata = app.archive_org.pending.clone().unwrap();
        assert!(
            matches!(&metadata.kind, ArchiveRequest::Details { identifier, open: true } if identifier == "fixture")
        );
        app.view.screen = Screen::History;
        app.view.status_line = "Other screen".into();
        app.handle_archive_response(
            metadata,
            Ok(ArchiveResponse::Details(
                super::super::tests::lookup_details("fixture"),
            )),
        );
        assert_eq!(app.view.status_line, "Other screen");
        assert!(app.archive_org.restoring.is_some());
        app.view.screen = Screen::ArchiveOrg;
        app.populate_archive_org();
        assert!(app.archive_org.restoring.is_none());
        assert_eq!(app.view.details_scroll, 12);
        assert_eq!(
            app.archive_org.active.as_ref().unwrap().tracks[app.view.selected].filename,
            "01 chapter.opus"
        );
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_back_row_selection_revokes_cold_restore_and_stale_metadata() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.archive_org.items = vec![super::super::tests::item()];
        app.archive_org.active = Some(super::super::tests::lookup_details("fixture"));
        app.search_archive_metadata("topic".into(), ArchiveOrgSearchScope::Topic);
        app.archive_org.history[0].cached = None;
        app.go_back_archive_org();
        let search = app.archive_org.pending.clone().unwrap();
        app.handle_archive_response(
            search,
            Ok(ArchiveResponse::Search(ArchiveOrgSearchPage {
                items: vec![super::super::tests::item()],
                page: 1,
                total: 1,
                next_page: None,
            })),
        );
        let stale = app.archive_org.pending.clone().unwrap();
        app.dispatch(UiAction::SelectRow(0));
        assert!(app.archive_org.restoring.is_none());
        app.handle_archive_response(
            stale,
            Ok(ArchiveResponse::Details(
                super::super::tests::lookup_details("fixture"),
            )),
        );
        assert!(app.archive_org.active.is_none());
        assert!(!app.view.archive_org_back_available);
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_back_does_not_revoke_a_pinned_download_lookup() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.archive_org.items = vec![super::super::tests::item()];
        app.search_archive_metadata("topic".into(), ArchiveOrgSearchScope::Topic);
        let source = url::Url::parse("https://archive.org/download/other/audio.opus").unwrap();
        assert!(app.archive_download_variants(&source).unwrap().is_none());
        assert!(!app.view.archive_org_back_available);
        let generation = app.archive_org.pending.as_ref().unwrap().generation;
        assert!(!app.go_back_archive_org());
        assert_eq!(
            app.archive_org.pending.as_ref().unwrap().generation,
            generation
        );
        assert_eq!(app.archive_org.history.len(), 1);
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_back_restores_nested_submitted_queries_rows_and_scroll() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.submit_archive_org_search("initial".into());
        app.archive_org.pending = None;
        app.archive_org.request = None;
        app.archive_org.items = vec![super::super::tests::item(), super::super::tests::item()];
        app.archive_org.items[1].identifier = "second".into();
        app.view.selected = 1;
        app.view.details_scroll = 17;
        app.search_archive_metadata("creator".into(), ArchiveOrgSearchScope::Creator);
        app.archive_org.pending = None;
        app.archive_org.request = None;
        app.archive_org.items = vec![super::super::tests::item()];
        app.search_archive_metadata("topic".into(), ArchiveOrgSearchScope::Topic);
        assert!(app.view.archive_org_back_available);
        assert!(app.go_back_archive_org());
        assert_eq!(app.archive_org.submitted_query, "creator");
        assert_eq!(
            app.archive_org.submitted_scope,
            ArchiveOrgSearchScope::Creator
        );
        assert!(app.go_back_archive_org());
        assert_eq!(app.archive_org.submitted_query, "initial");
        assert_eq!(app.view.selected, 1);
        assert_eq!(app.view.details_scroll, 17);
        assert!(!app.view.archive_org_back_available);
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_back_restores_open_track_then_esc_returns_to_its_catalogue() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.archive_org.items = vec![super::super::tests::item()];
        app.archive_org.active = Some(super::super::tests::lookup_details("fixture"));
        app.view.details_scroll = 9;
        app.search_archive_metadata("History".into(), ArchiveOrgSearchScope::Topic);
        assert!(app.go_back_archive_org());
        assert_eq!(
            app.archive_org.active.as_ref().unwrap().tracks[0].filename,
            "01 chapter.opus"
        );
        assert_eq!(app.view.details_scroll, 9);
        assert!(app.go_back_archive_org());
        assert!(app.archive_org.active.is_none());
        assert!(!app.view.archive_org_back_available);
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_back_plain_search_clears_history_and_stale_response_cannot_replace_restore() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.archive_org.items = vec![super::super::tests::item()];
        app.search_archive_metadata("topic".into(), ArchiveOrgSearchScope::Topic);
        let stale = app.archive_org.pending.clone().unwrap();
        assert!(app.go_back_archive_org());
        app.handle_archive_response(
            stale,
            Ok(ArchiveResponse::Search(ArchiveOrgSearchPage {
                items: Vec::new(),
                page: 1,
                total: 0,
                next_page: None,
            })),
        );
        assert_eq!(app.archive_org.items.len(), 1);
        app.search_archive_metadata("other".into(), ArchiveOrgSearchScope::Topic);
        app.submit_archive_org_search("fresh".into());
        assert!(app.archive_org.history.is_empty());
        assert!(!app.view.archive_org_back_available);
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }

    #[test]
    fn archive_back_cache_eviction_keeps_logical_route_and_reloads_bounded_pages() {
        let (_temporary, mut app) = controller();
        super::super::tests::occupy_archive_worker(&mut app);
        app.archive_org.submitted_query = "original".into();
        app.archive_org_search_query = "original".into();
        let make_item = |index: usize, large: bool| {
            let mut item = super::super::tests::item();
            item.identifier = format!("item-{index}");
            item.description = Some(if large {
                "x".repeat(60_000)
            } else {
                String::new()
            });
            item
        };
        app.archive_org.items = (0..100).map(|index| make_item(index, true)).collect();
        app.view.selected = 55;
        app.view.details_scroll = 4;
        app.search_archive_metadata("topic".into(), ArchiveOrgSearchScope::Topic);
        assert_eq!(app.archive_org.history.len(), 1);
        assert!(app.archive_org.history[0].cached.is_none());
        assert!(app.go_back_archive_org());
        let first = app.archive_org.pending.clone().unwrap();
        app.handle_archive_response(
            first,
            Ok(ArchiveResponse::Search(ArchiveOrgSearchPage {
                items: (0..50).map(|index| make_item(index, false)).collect(),
                page: 1,
                total: 100,
                next_page: Some(2),
            })),
        );
        let second = app.archive_org.pending.clone().unwrap();
        assert!(
            matches!(&second.kind, ArchiveRequest::Search(request) if request.page == 2 && request.query == "original")
        );
        app.handle_archive_response(
            second,
            Ok(ArchiveResponse::Search(ArchiveOrgSearchPage {
                items: (50..100).map(|index| make_item(index, false)).collect(),
                page: 2,
                total: 100,
                next_page: None,
            })),
        );
        assert_eq!(app.view.selected, 55);
        assert_eq!(app.view.details_scroll, 4);
        assert!(app.archive_org.restoring.is_none());
        for index in 0..MAX_LOCATIONS + 4 {
            app.search_archive_metadata(format!("hop-{index}"), ArchiveOrgSearchScope::Topic);
        }
        assert_eq!(app.archive_org.history.len(), MAX_LOCATIONS);
        assert!(
            app.archive_org
                .history
                .iter()
                .filter_map(|location| location.cached.as_ref())
                .map(|cached| cached.weight)
                .sum::<usize>()
                <= MAX_RETAINED_BYTES
        );
        app.archive_org
            .worker
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
    }
}
