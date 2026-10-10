//! Explicit description/comment lookups with bounded caching and worker ownership.

use super::*;
use crate::url_info::{UrlInfoClient, site_files};
use crate::view::{SiteFileEntryView, SiteFilePopupView, UrlInfoView};

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_URLS: usize = 32_768;
const CACHE_ENTRIES: usize = 64;
const SITE_FILE_HISTORY: usize = 8;

/// At most one request runs; newer explicit requests replace only the pending slot.
#[derive(Default)]
pub(super) struct UrlInfoState {
    owner: Option<(MediaId, String)>,
    entries: Vec<UrlInfoView>,
    offset: usize,
    pending: Option<url::Url>,
    pub(super) worker: Option<LookupWorker>,
    client: UrlInfoClient,
    cache: VecDeque<(url::Url, Vec<String>)>,
    site_file_pending: Option<(url::Url, bool)>,
    pub(super) site_file_worker: Option<SiteFileWorker>,
    site_file_history: VecDeque<SiteFilePopupView>,
    #[cfg(feature = "web-browser")]
    site_file_web_return: Option<SiteFileWebReturn>,
}

/// One move-only return point preserves the loaded sitemap when its page opens in Web.
#[cfg(feature = "web-browser")]
struct SiteFileWebReturn {
    popup: SiteFilePopupView,
    parents: VecDeque<SiteFilePopupView>,
    location: DetailNavigationSnapshot,
}

/// One explicit file request, retained after cancellation until its bounded worker finishes.
pub(super) struct SiteFileWorker {
    pub(super) url: url::Url,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) receiver: Receiver<Result<site_files::SiteFile, String>>,
}

/// A cancelled worker still returns its client before the next request can start.
pub(super) struct LookupWorker {
    pub(super) url: url::Url,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) receiver: Receiver<(UrlInfoClient, Vec<String>)>,
}

impl Drop for UrlInfoState {
    fn drop(&mut self) {
        self.cancel();
        self.cancel_site_file();
    }
}

impl UrlInfoState {
    /// Releases the URL body when another Details disclosure takes focus.
    pub(super) fn collapse(&mut self) {
        self.cancel();
        for entry in &mut self.entries {
            entry.expanded = false;
            entry.loading = false;
            // Closed projections cannot bypass the cache's memory bound.
            entry.lines.clear();
        }
    }

    /// Stops work between bounded network operations without blocking the UI.
    fn cancel(&mut self) {
        self.pending = None;
        if let Some(worker) = &self.worker {
            worker.cancelled.store(true, AtomicOrdering::Relaxed);
        }
    }

    /// Cancels the file request without joining its network worker on the UI thread.
    fn cancel_site_file(&mut self) {
        self.site_file_pending = None;
        if let Some(worker) = &self.site_file_worker {
            worker.cancelled.store(true, AtomicOrdering::Relaxed);
        }
    }

    /// Caches success and partial failures until eviction or process exit.
    pub(super) fn remember(&mut self, url: url::Url, lines: Vec<String>) {
        self.cache.retain(|(key, _)| key != &url);
        while self.cache.len() >= CACHE_ENTRIES {
            self.cache.pop_front();
        }
        self.cache.push_back((url, lines));
    }

    /// Reuses the last session result without a timer or automatic request.
    fn cached(&self, url: &url::Url) -> Option<Vec<String>> {
        self.cache
            .iter()
            .find(|(key, _)| key == url)
            .map(|(_, lines)| lines.clone())
    }
}

/// Extracts bounded, unique HTTP(S) URLs without changing original source bytes.
fn comment_urls(comment: &str) -> Vec<UrlInfoView> {
    if comment.len() > MAX_SOURCE_BYTES {
        return Vec::new();
    }
    let mut entries: Vec<UrlInfoView> = Vec::new();
    let mut seen = HashSet::new();
    for (start, end) in crate::links::description_url_ranges(comment) {
        if end - start > 4096 {
            continue;
        }
        let Ok(mut url) = url::Url::parse(&comment[start..end]) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            continue;
        }
        url.set_fragment(None);
        if !seen.insert(url.clone()) {
            continue;
        }
        entries.push(UrlInfoView {
            url: url.into(),
            ..UrlInfoView::default()
        });
        if entries.len() == MAX_URLS {
            break;
        }
    }
    entries
}

impl AppController {
    /// Reads existing `YouTube` descriptions or Local tags without probing files or URLs.
    fn selected_url_info_source(&self) -> Option<(MediaId, &str)> {
        let details = self.view.details.as_ref()?;
        let id = details.media_id.as_ref()?;
        if id.source == SourceKind::YouTube {
            return (details.description.len() <= MAX_SOURCE_BYTES)
                .then(|| (id.clone(), details.description.as_str()));
        }
        if self.view.screen != Screen::Local || self.view.local_browse_pending {
            return None;
        }
        if id.source != SourceKind::Local {
            return None;
        }
        let path = local_path_from_media_id(id)?;
        if self.selected_local_path().as_ref() != Some(&path) {
            return None;
        }
        let item = self
            .local_media_cache
            .get(&path)
            .map(|entry| &entry.item)
            .or_else(|| self.local_results.iter().find(|item| item.path == path))?;
        let comment = item.comment.as_deref()?;
        (comment.len() <= MAX_SOURCE_BYTES).then(|| (id.clone(), comment))
    }

    /// Projects existing facts and drains event-woken completions; never auto-fetches.
    pub(super) fn refresh_url_info(&mut self) {
        let same_owner = match (self.selected_url_info_source(), &self.url_info.owner) {
            (Some((id, comment)), Some((old_id, old_comment))) => {
                id == *old_id && comment == old_comment
            }
            (None, None) => true,
            _ => false,
        };
        if !same_owner {
            let owner = self
                .selected_url_info_source()
                .map(|(id, comment)| (id, comment.to_owned()));
            self.url_info.cancel();
            self.dismiss_site_file();
            self.url_info.entries = owner
                .as_ref()
                .map_or_else(Vec::new, |(_, text)| comment_urls(text));
            self.url_info.owner = owner;
            self.url_info.offset = 0;
        }
        let completion =
            self.url_info
                .worker
                .as_ref()
                .and_then(|worker| match worker.receiver.try_recv() {
                    Ok(value) => Some(value),
                    Err(TryRecvError::Disconnected) => Some((
                        UrlInfoClient::default(),
                        vec!["URL info lookup failed.".into()],
                    )),
                    Err(TryRecvError::Empty) => None,
                });
        if let Some((client, lines)) = completion {
            let worker = self.url_info.worker.take().expect("completed URL lookup");
            self.url_info.client = client;
            if !worker.cancelled.load(AtomicOrdering::Relaxed) {
                self.url_info.remember(worker.url.clone(), lines.clone());
                for entry in &mut self.url_info.entries {
                    if entry.expanded && entry.url == worker.url.as_str() {
                        entry.loading = false;
                        entry.lines.clone_from(&lines);
                    }
                }
            }
        }
        if self.url_info.worker.is_none()
            && let Some(url) = self.url_info.pending.take()
        {
            let mut client = std::mem::take(&mut self.url_info.client);
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancel = Arc::clone(&cancelled);
            let worker_url = url.clone();
            let (sender, receiver) = bounded(1);
            let sender = ResponseSender::new(sender, self.worker_notifier.clone());
            match thread::Builder::new()
                .name("youta-url-info".into())
                .spawn(move || {
                    let lines = client.lookup(&worker_url, &cancel);
                    let _ = sender.send((client, lines));
                }) {
                Ok(_) => {
                    self.url_info.worker = Some(LookupWorker {
                        url,
                        cancelled,
                        receiver,
                    });
                }
                Err(_) => {
                    for entry in &mut self.url_info.entries {
                        if entry.expanded {
                            entry.loading = false;
                            entry.lines = vec!["Could not start the URL info worker.".into()];
                        }
                    }
                }
            }
        }
        if let Some(details) = self.view.details.as_mut() {
            details.url_info.clone_from(&self.url_info.entries);
            details.url_info_offset = self.url_info.offset;
        }
        self.refresh_site_file();
    }

    /// Moves the compact terminal URL rail without starting a lookup.
    pub(super) fn move_url_info(&mut self, delta: i32) {
        self.refresh_url_info();
        self.url_info.offset = self
            .url_info
            .offset
            .saturating_add_signed(delta as isize)
            .min(self.url_info.entries.len().saturating_sub(1));
        self.refresh_url_info();
    }

    /// Opens one disclosure only on explicit input, reusing a session result if present.
    pub(super) fn toggle_url_info(&mut self, index: usize) {
        self.refresh_url_info();
        let Some(entry) = self.url_info.entries.get(index) else {
            return;
        };
        let expanded = !entry.expanded;
        let Ok(url) = url::Url::parse(&entry.url) else {
            return;
        };
        self.url_info.collapse();
        let lines = self.url_info.cached(&url);
        let entry = &mut self.url_info.entries[index];
        entry.expanded = expanded;
        if expanded {
            entry.loading = lines.is_none();
            entry.lines = lines.unwrap_or_default();
            if entry.loading {
                self.url_info.pending = Some(url);
            }
        }
        self.view.details_scroll = 0;
        self.view.details_text_selection = None;
        self.view.details_focused = true;
        if let Some(details) = &mut self.view.details {
            details.expanded_wikidata_item = None;
        }
        self.refresh_url_info();
    }

    /// Opens only an explicitly requested root-origin file for an expanded URL disclosure.
    pub(super) fn open_url_site_file(&mut self, index: usize, sitemap: bool) {
        self.refresh_url_info();
        let Some(entry) = self
            .url_info
            .entries
            .get(index)
            .filter(|entry| entry.expanded)
        else {
            return;
        };
        let Ok(origin) = url::Url::parse(&entry.url) else {
            return;
        };
        self.dismiss_site_file();
        #[cfg(feature = "web-browser")]
        {
            self.url_info.site_file_web_return = None;
        }
        match site_files::target(&origin, sitemap) {
            Ok(url) => self.queue_site_file(url, sitemap),
            Err(error) => {
                self.view.site_file_popup = Some(SiteFilePopupView {
                    title: if sitemap { "Sitemap" } else { "robots.txt" }.into(),
                    sitemap,
                    error: Some(error),
                    ..SiteFilePopupView::default()
                });
            }
        }
    }

    /// Schedules one selected file, coalescing behind a cancelled worker rather than spawning more.
    fn queue_site_file(&mut self, url: url::Url, sitemap: bool) {
        self.url_info.cancel_site_file();
        self.view.site_file_popup = Some(SiteFilePopupView {
            title: if sitemap { "Sitemap" } else { "robots.txt" }.into(),
            url: url.to_string(),
            sitemap,
            loading: true,
            can_go_back: !self.url_info.site_file_history.is_empty(),
            ..SiteFilePopupView::default()
        });
        self.url_info.site_file_pending = Some((url, sitemap));
        self.refresh_site_file();
    }

    /// Closes immediately and releases cached parent text; a late reply cannot reopen the dialog.
    pub(super) fn dismiss_site_file(&mut self) {
        self.url_info.cancel_site_file();
        self.url_info.site_file_history.clear();
        self.view.site_file_popup = None;
    }

    /// Restores a bounded in-memory parent, including its row selection, with no network request.
    pub(super) fn back_site_file(&mut self) {
        let Some(mut parent) = self.url_info.site_file_history.pop_back() else {
            return;
        };
        self.url_info.cancel_site_file();
        parent.can_go_back = !self.url_info.site_file_history.is_empty();
        self.view.site_file_popup = Some(parent);
    }

    /// Changes only the viewer row; arrow keys never fetch a sitemap or activate playback.
    pub(super) fn move_site_file_selection(&mut self, delta: i32) {
        if let Some(popup) = self
            .view
            .site_file_popup
            .as_mut()
            .filter(|popup| popup.sitemap && !popup.loading)
        {
            popup.selected = popup
                .selected
                .saturating_add_signed(isize::try_from(delta).unwrap_or_default())
                .min(popup.entries.len().saturating_sub(1));
            popup.scroll_offset = popup.selected;
            popup.entry_line_offset = 0;
        }
    }

    /// Opens child indexes inside the viewer and sends ordinary page URLs through the Web workflow.
    pub(super) fn activate_site_file_entry(&mut self, index: usize) {
        let Some(popup) = self
            .view
            .site_file_popup
            .as_ref()
            .filter(|popup| popup.sitemap && !popup.loading)
        else {
            return;
        };
        let Some(entry) = popup.entries.get(index) else {
            return;
        };
        let Ok(url) = url::Url::parse(&entry.url) else {
            return;
        };
        if popup.sitemap_index {
            let mut parent = popup.clone();
            parent.selected = index;
            while self.url_info.site_file_history.len() >= SITE_FILE_HISTORY {
                self.url_info.site_file_history.pop_front();
            }
            self.url_info.site_file_history.push_back(parent);
            self.queue_site_file(url, true);
        } else {
            #[cfg(feature = "web-browser")]
            {
                if let Err(error) = crate::web_browser::validate_web_url(&url) {
                    if let Some(popup) = self.view.site_file_popup.as_mut() {
                        popup.error = Some(error.to_string());
                    }
                    return;
                }
                let mut popup = self.view.site_file_popup.take().expect("selected sitemap");
                popup.selected = index;
                let parents = std::mem::take(&mut self.url_info.site_file_history);
                self.url_info.cancel_site_file();
                let location = self.take_detail_navigation_snapshot();
                let result = self.open_web_url(url);
                self.url_info.site_file_web_return = Some(SiteFileWebReturn {
                    popup,
                    parents,
                    location,
                });
                if let Err(error) = result {
                    self.restore_site_file_from_web();
                    if let Some(popup) = self.view.site_file_popup.as_mut() {
                        popup.error = Some(error.to_string());
                    }
                }
            }
            #[cfg(not(feature = "web-browser"))]
            {
                let _ = url;
                if let Some(popup) = self.view.site_file_popup.as_mut() {
                    popup.error = Some("This build does not include the Web tab.".into());
                }
            }
        }
    }

    /// Restores the exact loaded viewer after Web's own explicit child history is exhausted.
    #[cfg(feature = "web-browser")]
    pub(super) fn restore_site_file_from_web(&mut self) -> bool {
        if self.web_has_back_history() {
            return false;
        }
        let Some(saved) = self.url_info.site_file_web_return.take() else {
            return false;
        };
        self.cancel_web_navigation_for_now_playing();
        self.show_screen(saved.location.screen);
        self.restore_detail_navigation_snapshot(saved.location);
        self.refresh_url_info();
        self.url_info.site_file_history = saved.parents;
        self.view.site_file_popup = Some(saved.popup);
        true
    }

    /// Explicitly opening another Web address releases the previous route's return point.
    #[cfg(feature = "web-browser")]
    pub(super) fn discard_site_file_web_return(&mut self) {
        self.url_info.site_file_web_return = None;
    }

    /// Drains event-woken completion and starts at most one already requested file worker.
    fn refresh_site_file(&mut self) {
        let completion = self.url_info.site_file_worker.as_ref().and_then(|worker| {
            match worker.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("The site-file worker stopped unexpectedly.".into()))
                }
                Err(TryRecvError::Empty) => None,
            }
        });
        if let Some(result) = completion {
            let worker = self
                .url_info
                .site_file_worker
                .take()
                .expect("completed site-file worker");
            if !worker.cancelled.load(AtomicOrdering::Relaxed)
                && let Some(popup) = self
                    .view
                    .site_file_popup
                    .as_mut()
                    .filter(|popup| popup.url == worker.url.as_str())
            {
                popup.loading = false;
                match result {
                    Ok(file) => {
                        popup.url = file.url.to_string();
                        match file.content {
                            site_files::SiteFileContent::Robots(text) => popup.text = text,
                            site_files::SiteFileContent::Sitemap { index, entries } => {
                                popup.sitemap_index = index;
                                popup.entries = entries
                                    .into_iter()
                                    .map(|entry| SiteFileEntryView {
                                        url: entry.url.to_string(),
                                        metadata: entry.metadata,
                                    })
                                    .collect();
                            }
                        }
                    }
                    Err(error) => popup.error = Some(error),
                }
            }
        }
        if self.url_info.site_file_worker.is_some() {
            return;
        }
        let Some((url, sitemap)) = self.url_info.site_file_pending.take() else {
            return;
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancelled);
        let worker_url = url.clone();
        let (sender, receiver) = bounded(1);
        let sender = ResponseSender::new(sender, self.worker_notifier.clone());
        match thread::Builder::new()
            .name("youta-site-file".into())
            .spawn(move || {
                let result = if sitemap {
                    site_files::lookup_sitemap(&worker_url, &cancel)
                } else {
                    site_files::lookup(&worker_url, false, &cancel)
                };
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.url_info.site_file_worker = Some(SiteFileWorker {
                    url,
                    cancelled,
                    receiver,
                })
            }
            Err(_) => {
                if let Some(popup) = self.view.site_file_popup.as_mut() {
                    popup.loading = false;
                    popup.error = Some("Could not start the site-file worker.".into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_info_urls_keep_unicode_boundaries_and_deduplicate_fragments() {
        let entries = comment_urls(
            "Музыка (https://example.com/song#one). https://example.com/song#two ftp://example.com/a https://example.org/",
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.url.as_str())
                .collect::<Vec<_>>(),
            ["https://example.com/song", "https://example.org/"]
        );
        assert!(comment_urls(&"x".repeat(MAX_SOURCE_BYTES + 1)).is_empty());
        assert_eq!(
            comment_urls(
                &(0..100)
                    .map(|i| format!("https://example.com/{i} "))
                    .collect::<String>()
            )
            .len(),
            100
        );
    }

    #[test]
    fn url_info_cache_is_bounded_and_reused_until_exit() {
        let mut state = UrlInfoState::default();
        for i in 0..100 {
            state.remember(
                url::Url::parse(&format!("https://example.com/{i}")).unwrap(),
                vec![i.to_string()],
            );
        }
        assert_eq!(state.cache.len(), CACHE_ENTRIES);
        let (url, lines) = state.cache.back().unwrap();
        assert_eq!(state.cached(url).as_ref(), Some(lines));
        assert!(
            state
                .cached(&url::Url::parse("https://example.com/0").unwrap())
                .is_none()
        );
    }
}
