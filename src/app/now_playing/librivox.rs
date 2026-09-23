//! Exact book/section recovery for accepted LibriVox playback without a cached source page.

use super::*;

/// Only this accepted media identity may consume the exact book lookup.
pub(super) struct PendingReveal {
    generation: u64,
    id: MediaId,
    book_id: u64,
    section_id: u64,
}

impl AppController {
    /// Requests only the canonical playing book using the existing bounded provider lane.
    pub(in crate::app) fn request_playing_librivox(&mut self, item: &QueueItem) -> bool {
        let Some((book_id, section_id)) =
            parse_librivox_section_external_id(&item.media.id.external_id)
        else {
            return false;
        };
        if self.current_media.as_ref() != Some(&item.media.id) {
            return false;
        }
        if self
            .now_playing_navigation
            .pending_librivox
            .as_ref()
            .is_some_and(|pending| {
                pending.id == item.media.id && pending.generation == self.librivox_generation
            })
        {
            return true;
        }
        self.cancel_librivox_now_playing_navigation();
        self.prepare_screen_transition(Screen::LibriVox);
        self.view.screen = Screen::LibriVox;
        self.view
            .search_query
            .clone_from(&self.librivox_search_query);
        self.view.selected = self.librivox_selected;
        self.push_librivox_navigation_snapshot();
        self.librivox_route = LibrivoxRoute::Book;
        self.active_librivox_book = None;
        self.active_librivox_author = None;
        self.librivox_generation = self.librivox_generation.wrapping_add(1);
        let generation = self.librivox_generation;
        self.pending_librivox_request = Some(PendingLibrivoxRequest::Book {
            generation,
            book_id,
        });
        self.now_playing_navigation.pending_librivox = Some(PendingReveal {
            generation,
            id: item.media.id.clone(),
            book_id,
            section_id,
        });
        self.view.rows.clear();
        self.view.selected = 0;
        self.view.details = Some(detail_from_media_item(
            &item.media,
            self.effective_youtube_thumbnail_size(),
        ));
        self.view.right_panel_mode = RightPanelMode::Details;
        self.view.details_focused = true;
        if !self.send_provider_request(
            ProviderRequest::LibrivoxBook {
                generation,
                book_id,
            },
            "Could not load the playing LibriVox book",
        ) {
            self.cancel_librivox_now_playing_navigation();
            self.librivox_navigation_back.pop_back();
            return true;
        }
        self.begin_search_activity(SearchActivity::LibriVox);
        self.view.status_line = format!("Loading the book for {}…", item.media.title);
        true
    }

    /// Applies only an exact response whose playback identity and visible route still agree.
    pub(in crate::app) fn finish_playing_librivox(
        &mut self,
        generation: u64,
        book_id: u64,
        result: &Result<Box<LibrivoxBook>, String>,
    ) -> bool {
        let Some(owner) = self
            .now_playing_navigation
            .pending_librivox
            .as_ref()
            .filter(|owner| owner.generation == generation && owner.book_id == book_id)
        else {
            return false;
        };
        let current = self.librivox_generation == generation
            && self.view.screen == Screen::LibriVox
            && !self.view.search_editing
            && self.current_media.as_ref() == Some(&owner.id)
            && matches!(self.pending_librivox_request, Some(PendingLibrivoxRequest::Book { generation: current_generation, book_id: current_book }) if current_generation == generation && current_book == book_id);
        let owner = self
            .now_playing_navigation
            .pending_librivox
            .take()
            .expect("checked above");
        if !current {
            if self.librivox_generation == generation {
                self.pending_librivox_request = None;
                self.finish_search_activity(SearchActivity::LibriVox);
            }
            return true;
        }
        self.pending_librivox_request = None;
        self.finish_search_activity(SearchActivity::LibriVox);
        let book = match result {
            Ok(book) if book.book_id == book_id => book,
            Ok(_) => {
                self.view.status_line =
                    "LibriVox returned a different book; playback is unchanged".into();
                return true;
            }
            Err(_) => {
                self.view.status_line =
                    "Could not load the playing LibriVox book; playback is unchanged".into();
                return true;
            }
        };
        let selected = book
            .sections
            .iter()
            .position(|section| section.section_id == owner.section_id);
        self.active_librivox_book = Some((**book).clone());
        self.active_librivox_author = None;
        self.librivox_route = LibrivoxRoute::Book;
        self.librivox_selected = selected.unwrap_or_default();
        self.view.selected = self.librivox_selected;
        self.populate_librivox();
        if selected.is_some() {
            if let Some(item) = self
                .playback_queue
                .current()
                .cloned()
                .filter(|item| item.media.id == owner.id)
            {
                self.remember_now_playing_context(&item);
                self.finish_now_playing_selection(&item);
            }
        } else {
            self.view.details = None;
            self.view.status_line =
                "This book no longer contains the playing section; playback is unchanged".into();
        }
        true
    }

    /// Removes this navigation owner without cancelling unrelated source work.
    pub(in crate::app) fn cancel_librivox_now_playing_navigation(&mut self) {
        let Some(owner) = self.now_playing_navigation.pending_librivox.take() else {
            return;
        };
        if self.librivox_generation == owner.generation {
            self.librivox_generation = self.librivox_generation.wrapping_add(1);
            self.pending_librivox_request = None;
            self.finish_search_activity(SearchActivity::LibriVox);
        }
    }

    /// A newly accepted different track revokes the previous title's metadata lookup.
    pub(in crate::app) fn cancel_changed_librivox_now_playing_navigation(&mut self, id: &MediaId) {
        if self
            .now_playing_navigation
            .pending_librivox
            .as_ref()
            .is_some_and(|owner| &owner.id != id)
        {
            self.cancel_librivox_now_playing_navigation();
        }
    }
}
