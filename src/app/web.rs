//! Session-only Web navigation and bounded background directory fetching.

use super::*;
use crate::web_browser::{
    WebBrowserClient, WebDirectoryListing, WebEntry, WebEntryKind, validate_web_url,
};
use sha2::{Digest, Sha256};

/// One in-flight request plus one replaceable latest request, never an unbounded queue.
#[derive(Default)]
pub(super) struct WebState {
    pub(super) listing: Option<WebDirectoryListing>,
    pub(super) query: String,
    pub(super) selected: usize,
    pub(super) generation: u64,
    pub(super) pending: bool,
    pub(super) worker: Option<WebWorker>,
    #[cfg(feature = "local-metadata")]
    pub(super) metadata: super::web_metadata::WebMetadataState,
    request: Option<(u64, url::Url)>,
    back: VecDeque<(url::Url, usize)>,
    restore_row: usize,
    message: String,
}

/// A deadline-bounded worker owns no controller or persistence references.
pub(super) struct WebWorker {
    generation: u64,
    response: Receiver<Result<WebDirectoryListing, String>>,
    thread: JoinHandle<()>,
}

impl AppController {
    /// Revokes queued and in-flight directory ownership before restoring a typed source page.
    pub(super) fn cancel_web_navigation_for_now_playing(&mut self) {
        self.web.generation = self.web.generation.wrapping_add(1);
        self.web.request = None;
        self.web.pending = false;
    }

    /// Opens an explicitly supplied startup URL through the normal Web worker.
    ///
    /// Navigation is session-only and never activates a media row or playback.
    /// Local and private HTTP servers follow the same policy as the URL editor.
    ///
    /// # Errors
    ///
    /// Returns an error for credentials, unsupported schemes, or an invalid URL.
    pub fn open_web_url(
        &mut self,
        url: url::Url,
    ) -> Result<(), crate::web_browser::WebBrowserError> {
        validate_web_url(&url)?;
        self.show_screen(Screen::Web);
        self.web.back.clear();
        self.browse_web_url(url, 0);
        Ok(())
    }

    /// Validates an explicit address before scheduling any network work.
    pub(super) fn submit_web_url(&mut self) {
        let result = url::Url::parse(self.view.search_query.trim())
            .map_err(|_| "Enter a complete http:// or https:// URL".to_owned())
            .and_then(|url| {
                validate_web_url(&url)
                    .map(|()| url)
                    .map_err(|error| error.to_string())
            });
        match result {
            Ok(url) => {
                self.web.back.clear();
                self.browse_web_url(url, 0);
            }
            Err(message) => {
                self.view.status_line = message;
                self.view.search_editing = true;
            }
        }
    }

    /// Replaces pending work while leaving the sole running request deadline-bounded.
    fn browse_web_url(&mut self, mut url: url::Url, restore_row: usize) {
        url.set_fragment(None);
        if let Err(error) = validate_web_url(&url) {
            self.view.status_line = error.to_string();
            return;
        }
        self.web.generation = self.web.generation.wrapping_add(1);
        self.web.query = url.to_string();
        self.web.request = Some((self.web.generation, url));
        self.web.pending = true;
        self.web.restore_row = restore_row;
        self.web.listing = None;
        self.web.selected = 0;
        "Loading Web folder… Audio only".clone_into(&mut self.web.message);
        self.view.search_editing = false;
        self.populate_web();
        self.start_web_worker();
    }

    /// Starts at most one HTTP fetch; repeated navigation coalesces to its latest URL.
    fn start_web_worker(&mut self) {
        if self.web.worker.is_some() {
            return;
        }
        let Some((generation, url)) = self.web.request.take() else {
            return;
        };
        let (sender, response) = bounded(1);
        match thread::Builder::new()
            .name("youta-web".to_owned())
            .spawn(move || {
                let result = WebBrowserClient::default()
                    .list(&url)
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
            }) {
            Ok(thread) => {
                self.web.worker = Some(WebWorker {
                    generation,
                    response,
                    thread,
                });
            }
            Err(_) => self.handle_web_response(
                generation,
                Err("Could not start the Web directory worker".to_owned()),
            ),
        }
    }

    /// Drains only completed work, without waiting in the rendering loop.
    pub(super) fn poll_web_worker(&mut self) {
        if self
            .web
            .worker
            .as_ref()
            .is_some_and(|worker| worker.thread.is_finished())
        {
            let worker = self.web.worker.take().expect("finished Web worker");
            let _ = worker.thread.join();
            let result = worker
                .response
                .try_recv()
                .unwrap_or_else(|_| Err("Web directory worker stopped unexpectedly".to_owned()));
            self.handle_web_response(worker.generation, result);
        }
        self.start_web_worker();
        #[cfg(feature = "local-metadata")]
        self.poll_web_metadata();
    }

    /// Accepts only the latest navigation owner, caching hidden-tab results without drawing them.
    pub(super) fn handle_web_response(
        &mut self,
        generation: u64,
        result: Result<WebDirectoryListing, String>,
    ) {
        if generation != self.web.generation || !self.web.pending {
            return;
        }
        self.web.pending = false;
        match result {
            Ok(listing) => {
                let count = listing
                    .entries
                    .iter()
                    .filter(|entry| entry.kind != WebEntryKind::Directory)
                    .count();
                self.web.message = if listing.truncated {
                    format!("{count} media files · listing limit reached · Audio only")
                } else if listing.entries.is_empty() {
                    "No supported audio/video links found; press / to open another URL".to_owned()
                } else {
                    format!("{count} media files · Audio only · [A] Autoplay follows this folder")
                };
                self.web.query = listing.url.to_string();
                self.web.selected = self.web.restore_row;
                self.web.listing = Some(listing);
            }
            Err(error) => {
                self.web.message = format!("Could not open Web folder: {error}");
                self.web.listing = None;
            }
        }
        if self.view.screen == Screen::Web {
            self.populate_web();
            self.refresh_selected_playlist_state();
        }
    }

    /// Projects compact local-style rows without exposing filesystem operations.
    pub(super) fn populate_web(&mut self) {
        if self.view.screen != Screen::Web {
            return;
        }
        if !self.view.search_editing {
            self.view.search_query.clone_from(&self.web.query);
            self.view.search_cursor_byte = self.view.search_query.len();
        }
        self.view.rows.clear();
        if let Some(listing) = &self.web.listing {
            if listing.parent.is_some() {
                self.view.rows.push(RowView {
                    title: "..".to_owned(),
                    source: "Web".to_owned(),
                    compact: true,
                    hide_watched_marker: true,
                    ..RowView::default()
                });
            }
            self.view
                .rows
                .extend(listing.entries.iter().map(|entry| RowView {
                    media_id: queue_item_from_web(entry).map(|item| item.media.id),
                    title: if entry.kind == WebEntryKind::Directory {
                        format!("{}/", entry.name.trim_end_matches('/'))
                    } else {
                        entry.name.clone()
                    },
                    source: "Web".to_owned(),
                    compact: true,
                    hide_watched_marker: entry.kind == WebEntryKind::Directory,
                    ..RowView::default()
                }));
            hydrate_row_playback_progress(&self.store, &mut self.view.rows);
        }
        self.view.selected = self
            .web
            .selected
            .min(self.view.rows.len().saturating_sub(1));
        self.web.selected = self.view.selected;
        self.update_web_detail();
        self.view.status_line = if self.web.query.is_empty() {
            self.view.search_editing = true;
            "Enter an HTTP(S) folder URL, then press Enter · Audio only".to_owned()
        } else {
            self.web.message.clone()
        };
        if self.web.pending {
            self.begin_search_activity(SearchActivity::Web);
        } else {
            self.finish_search_activity(SearchActivity::Web);
        }
    }

    /// Returns a selected child, excluding the separately synthesized parent row.
    pub(super) fn selected_web_entry(&self) -> Option<&WebEntry> {
        if self.view.screen != Screen::Web || self.web.pending {
            return None;
        }
        let listing = self.web.listing.as_ref()?;
        let index = self
            .view
            .selected
            .checked_sub(usize::from(listing.parent.is_some()))?;
        listing.entries.get(index)
    }

    /// Keeps Web Details tied only to the visible URL row.
    pub(super) fn update_web_detail(&mut self) {
        self.web.selected = self.view.selected;
        self.view.details = self.selected_web_entry().map(|entry| DetailView {
            media_id: queue_item_from_web(entry).map(|item| item.media.id),
            title: entry.name.clone(),
            source: "Web".to_owned(),
            webpage_url: Some(entry.url.clone()),
            description: if entry.kind == WebEntryKind::Directory {
                "Press Enter to open this folder.".to_owned()
            } else { "Audio only. With Autoplay enabled, playback continues through this folder in the displayed order.".to_owned() },
            ..DetailView::default()
        });
        #[cfg(feature = "local-metadata")]
        self.apply_web_metadata_detail();
    }

    /// Opens a directory or captures the playable list before starting direct media.
    pub(super) fn activate_web_selection(&mut self) {
        if self.web.pending {
            return;
        }
        let Some(listing) = &self.web.listing else {
            self.begin_search_input();
            return;
        };
        if self.view.selected == 0 && listing.parent.is_some() {
            self.open_web_parent();
            return;
        }
        let Some(entry) = self.selected_web_entry().cloned() else {
            return;
        };
        if entry.kind == WebEntryKind::Directory {
            self.web
                .back
                .push_back((listing.url.clone(), self.view.selected));
            while self.web.back.len() > 64 {
                self.web.back.pop_front();
            }
            self.browse_web_url(entry.url, 0);
        } else if let Some(item) = queue_item_from_web(&entry) {
            self.play_queue_item(item, false);
        }
    }

    /// Navigates to a real parent, restoring the child selection when it is known.
    pub(super) fn open_web_parent(&mut self) {
        if let Some((url, row)) = self.web.back.pop_back() {
            self.browse_web_url(url, row);
        } else if let Some(url) = self
            .web
            .listing
            .as_ref()
            .and_then(|listing| listing.parent.clone())
        {
            self.browse_web_url(url, 0);
        }
    }

    /// Refreshes the accepted location without changing its current selected row.
    pub(super) fn refresh_web(&mut self) {
        if self.view.screen != Screen::Web {
            return;
        }
        #[cfg(feature = "local-metadata")]
        self.invalidate_web_metadata();
        if let Ok(url) = url::Url::parse(&self.web.query) {
            self.browse_web_url(url, self.view.selected);
        } else {
            self.begin_search_input();
        }
    }

    /// Captures a bounded playback snapshot independent of later tab or folder changes.
    pub(super) fn web_autoplay_origin(&self, id: &MediaId) -> Option<AutoplayOrigin> {
        let listing = self.web.listing.as_ref()?;
        let items: Vec<_> = listing
            .entries
            .iter()
            .filter_map(queue_item_from_web)
            .collect();
        let index = items.iter().position(|item| item.media.id == *id)?;
        Some(AutoplayOrigin::Web {
            items: items.into(),
            index,
        })
    }
}

/// Retains exact signed links only in the ephemeral queue, not persisted identity.
pub(super) fn queue_item_from_web(entry: &WebEntry) -> Option<QueueItem> {
    let kind = match entry.kind {
        WebEntryKind::Directory => return None,
        WebEntryKind::Audio => MediaKind::Audio,
        WebEntryKind::Video => MediaKind::Video,
    };
    Some(QueueItem {
        media: MediaItem {
            id: MediaId::new(
                SourceKind::RemoteFiles,
                format!(
                    "web:sha256:{:x}",
                    Sha256::digest(entry.url.as_str().as_bytes())
                ),
            ),
            kind,
            title: entry.name.clone(),
            creator: None,
            description: None,
            webpage_url: entry.url.clone(),
            thumbnail_url: None,
            duration_seconds: None,
            published_at: None,
            statistics: MediaStatistics::default(),
            chapters: Vec::new(),
            captions: Vec::new(),
            license: MediaLicense::Unknown,
        },
        playback_location: entry.url.to_string(),
        start_at_seconds: None,
        added_at: unix_time(),
    })
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    /// Owns an isolated controller without provider or playback network work.
    fn controller() -> (tempfile::TempDir, AppController) {
        let directory = tempfile::tempdir().expect("startup state directory");
        let config = Config::for_dir(directory.path());
        let store = StateStore::open(&config).expect("startup state");
        (directory, AppController::new(config, store, None, None))
    }

    /// Startup queues the exact latest page through the existing worker, never playback.
    #[test]
    fn startup_web_url_schedules_latest_page_without_playing() {
        let (_directory, mut controller) = controller();
        controller.view.search_query = "saved YouTube search".into();
        controller.config.playback.autoplay = true;
        controller.view.autoplay = true;
        let (release, held) = bounded::<()>(1);
        let (_reply, response) = bounded(1);
        controller.web.worker = Some(WebWorker {
            generation: 0,
            response,
            thread: thread::spawn(move || {
                let _ = held.recv_timeout(Duration::from_secs(2));
            }),
        });
        let first = url::Url::parse("http://127.0.0.1:8000/first/#track").expect("local URL");
        controller.open_web_url(first).expect("open first page");
        let latest =
            url::Url::parse("http://192.168.1.20:8000/music/?view=all#track").expect("LAN URL");
        controller
            .open_web_url(latest.clone())
            .expect("replace startup page");
        let mut requested = latest;
        requested.set_fragment(None);
        assert_eq!(controller.view.screen, Screen::Web);
        assert_eq!(controller.youtube_search_query, "saved YouTube search");
        assert_eq!(controller.view.search_query, requested.as_str());
        assert!(!controller.view.search_editing);
        assert!(controller.web.pending);
        assert!(controller.web.back.is_empty());
        assert_eq!(
            controller
                .web
                .worker
                .as_ref()
                .expect("sole held worker")
                .generation,
            0
        );
        let (generation, url) = controller
            .web
            .request
            .take()
            .expect("latest pending request");
        assert_eq!(generation, 2);
        assert_eq!(url, requested);
        controller.handle_web_response(
            generation,
            Ok(WebDirectoryListing {
                parent: None,
                entries: vec![WebEntry {
                    url: requested.join("song.opus").expect("media URL"),
                    name: "song.opus".into(),
                    kind: WebEntryKind::Audio,
                }],
                url: requested,
                truncated: false,
            }),
        );
        assert_eq!(controller.view.rows.len(), 1);
        assert_eq!(controller.view.rows[0].title, "song.opus");
        assert!(!controller.web.pending);
        assert!(controller.player.is_none());
        assert!(controller.current_media.is_none());
        assert!(controller.current_autoplay_origin.is_none());
        drop(release);
        controller
            .web
            .worker
            .take()
            .expect("held worker")
            .thread
            .join()
            .expect("mock worker");
    }

    /// Validation precedes any tab mutation, worker creation, or network request.
    #[test]
    fn startup_web_url_rejects_credentials_and_non_http_before_navigation() {
        let (_directory, mut controller) = controller();
        let screen = controller.view.screen;
        let query = controller.view.search_query.clone();
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/",
            "https://user:secret@example.com/",
        ] {
            let url = url::Url::parse(raw).expect("invalid-source fixture");
            assert!(controller.open_web_url(url).is_err());
            assert_eq!(controller.view.screen, screen);
            assert_eq!(controller.view.search_query, query);
            assert!(controller.web.worker.is_none());
            assert!(controller.web.request.is_none());
        }
    }
}
