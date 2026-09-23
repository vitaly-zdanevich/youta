//! Reveals authoritative Archive metadata without replaying or downloading media.

use super::*;

/// One exact-file navigation intent, separate from ordinary metadata selection.
pub(super) struct PendingArchiveNowPlaying {
    generation: u64,
    id: MediaId,
    selected: usize,
}

impl AppController {
    /// Requests missing public item metadata for an exact accepted playback identity.
    pub(in crate::app) fn request_playing_archive_org(&mut self, id: &MediaId) -> bool {
        if id.source != SourceKind::ArchiveOrg || self.current_media.as_ref() != Some(id) {
            return false;
        }
        let Some(identifier) = url::Url::parse(&id.external_id)
            .ok()
            .and_then(|source| archive_download_identifier(&source))
        else {
            return false;
        };
        self.cancel_stale_archive_now_playing_navigation();
        if self
            .archive_org
            .now_playing
            .as_ref()
            .is_some_and(|owner| owner.id == *id)
        {
            return true;
        }
        if self.archive_download_lookup_pending() {
            self.view.status_line =
                "Finish the current archive.org download choice, then try again".into();
            return true;
        }
        self.cancel_archive_now_playing_navigation();
        // Enter the existing source without triggering its initial catalogue search.
        // Real prior rows remain visible until the exact metadata response arrives.
        self.archive_org.initialized = true;
        self.archive_org.restoring = None;
        self.archive_org.restart = None;
        self.show_screen(Screen::ArchiveOrg);
        self.queue_archive_request(
            ArchiveRequest::Details {
                identifier,
                open: true,
            },
            false,
        );
        if let Some(job) = self.archive_org.pending.as_ref() {
            self.archive_org.now_playing = Some(PendingArchiveNowPlaying {
                generation: job.generation,
                id: id.clone(),
                selected: self.view.selected,
            });
            self.begin_search_activity(SearchActivity::ArchiveOrg);
            self.view.status_line = "Locating playing archive.org file…".into();
            self.update_archive_back_available();
        }
        true
    }

    /// Revokes only a metadata lookup started by the now-playing action.
    pub(in crate::app) fn cancel_archive_now_playing_navigation(&mut self) {
        let Some(owner) = self.archive_org.now_playing.take() else {
            return;
        };
        if self
            .archive_org
            .pending
            .as_ref()
            .is_some_and(|job| job.generation == owner.generation)
        {
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
            self.finish_search_activity(SearchActivity::ArchiveOrg);
        }
        self.update_archive_back_available();
    }

    /// Selection, editing, and playback changes cannot reacquire an obsolete reveal.
    pub(super) fn cancel_stale_archive_now_playing_navigation(&mut self) {
        if self.archive_org.now_playing.as_ref().is_some_and(|owner| {
            self.view.screen != Screen::ArchiveOrg
                || self.current_media.as_ref() != Some(&owner.id)
                || self.archive_org.generation != owner.generation
                || self.view.selected != owner.selected
                || self.view.search_editing
        }) {
            self.cancel_archive_now_playing_navigation();
        }
    }

    /// Consumes only the matching reveal response; stale results remain cache-only.
    pub(super) fn complete_archive_now_playing_lookup(
        &mut self,
        job: &ArchiveJob,
        result: &Result<ArchiveResponse, String>,
    ) -> bool {
        if self
            .archive_org
            .now_playing
            .as_ref()
            .is_none_or(|owner| owner.generation != job.generation)
        {
            return false;
        }
        self.cancel_stale_archive_now_playing_navigation();
        let Some(owner) = self.archive_org.now_playing.take() else {
            return true;
        };
        self.archive_org.pending = None;
        self.archive_org.request = None;
        self.finish_search_activity(SearchActivity::ArchiveOrg);
        match result {
            Ok(ArchiveResponse::Details(_)) if self.reveal_playing_archive_org(&owner.id) => {
                if let Some(item) = self
                    .playback_queue
                    .items
                    .iter()
                    .find(|item| item.media.id == owner.id)
                    .cloned()
                {
                    self.finish_now_playing_selection(&item);
                }
            }
            Ok(_) => {
                self.view.status_line =
                    "The playing archive.org file is not listed in the current item metadata"
                        .into();
            }
            Err(error) => {
                self.view.status_line =
                    format!("Archive.org: could not locate the playing file: {error}")
            }
        }
        self.update_archive_back_available();
        true
    }

    /// Selects an exact known file or variant within its authoritative track family.
    pub(in crate::app) fn reveal_playing_archive_org(&mut self, id: &MediaId) -> bool {
        let Ok(source) = url::Url::parse(&id.external_id) else {
            return false;
        };
        if id.source != SourceKind::ArchiveOrg || !is_direct_audio_url(&source) {
            return false;
        }
        let selected = self
            .archive_org
            .active
            .as_ref()
            .and_then(|details| matching_track(details, &source))
            .or_else(|| {
                self.archive_org.cache.iter().rev().find_map(|(_, result)| {
                    result
                        .as_ref()
                        .ok()
                        .and_then(|details| matching_track(details, &source))
                })
            })
            .or_else(|| match self.current_autoplay_origin.as_ref() {
                Some(AutoplayOrigin::ArchiveOrg { details, .. }) => {
                    matching_track(details, &source)
                }
                _ => None,
            });
        let Some((details, index)) = selected else {
            return false;
        };
        let same_item = self
            .archive_org
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &details));
        if !same_item {
            // Hidden tabs retain their own selection, not another screen's row.
            let visible_selection = self.view.selected;
            if self.view.screen != Screen::ArchiveOrg {
                self.view.selected = self.archive_org_selected;
            }
            self.remember_archive_location();
            self.view.selected = visible_selection;
            self.archive_org.items = vec![details.item.clone()];
            self.archive_org.search_selected = 0;
            self.archive_org.total = 1;
            self.archive_org.next_page = None;
            self.archive_org.page_limit = None;
            self.archive_org.page_turn = None;
            // Retained playback metadata may have outlived cache eviction. Keep
            // the real enclosing item available when Esc leaves its track list.
            self.archive_org
                .cache
                .retain(|(identifier, _)| *identifier != details.item.identifier);
            self.archive_org
                .cache
                .push_back((details.item.identifier.clone(), Ok(Arc::clone(&details))));
            while self.archive_org.cache.len() > 8 {
                self.archive_org.cache.pop_front();
            }
            self.archive_org.active = Some(details);
        }
        // Navigation can revoke a stale search/open, but a pinned manual download
        // lookup owns independent work and must finish into its existing cache.
        if !self.archive_download_lookup_pending() {
            self.archive_org.generation = self.archive_org.generation.wrapping_add(1);
            self.archive_org.pending = None;
            self.archive_org.request = None;
        }
        self.archive_org.restoring = None;
        self.archive_org.restart = None;
        self.archive_org.initialized = true;
        self.archive_org_selected = index;
        if self.view.screen == Screen::ArchiveOrg {
            // show_screen saves the visible selection before projecting its target.
            self.view.selected = index;
        }
        self.show_screen(Screen::ArchiveOrg);
        true
    }
}

/// Matches only a canonical file belonging to this retained item's admitted family.
fn matching_track(
    details: &Arc<ArchiveOrgItemDetails>,
    source: &url::Url,
) -> Option<(Arc<ArchiveOrgItemDetails>, usize)> {
    if source.path_segments()?.nth(1)? != details.item.identifier {
        return None;
    }
    details
        .tracks
        .iter()
        .position(|track| {
            track.download_url == *source
                || track
                    .download_variants
                    .iter()
                    .any(|variant| variant.download_url == *source)
        })
        .map(|index| (Arc::clone(details), index))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses retained provider metadata rather than constructing a queue-only row.
    fn details(identifier: &str) -> Arc<ArchiveOrgItemDetails> {
        let mut details = (*super::super::tests::lookup_details(identifier)).clone();
        let mut second = details.tracks[0].clone();
        second.filename = "second.mp3".into();
        second.title = "Second track".into();
        second.download_url = url::Url::parse(&format!(
            "https://archive.org/download/{identifier}/second.mp3"
        ))
        .unwrap();
        second.download_variants.clear();
        details.tracks.push(second);
        Arc::new(details)
    }

    /// Supplies fresh History replay without any retained provider metadata.
    fn playing_controller() -> (tempfile::TempDir, AppController, MediaId) {
        let (directory, mut controller) = super::super::tests::lookup_controller();
        let details = details("playing");
        let mut item = queue_item(&details.item, &details.tracks[0]);
        let source = url::Url::parse("https://archive.org/download/playing/movie.mp4").unwrap();
        item.media.id = MediaId::new(SourceKind::ArchiveOrg, source.as_str());
        item.media.webpage_url = source.clone();
        item.playback_location = source.to_string();
        let id = item.media.id.clone();
        controller.current_media = Some(id.clone());
        controller.playback_queue.push(item);
        (directory, controller, id)
    }

    /// Existing public file variants, including original video, retain exact family identity.
    fn fresh_metadata() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "metadata": {"identifier": "playing", "title": "Playing album", "mediatype": "audio"},
            "files": [
                {"name":"first.flac", "source":"original"},
                {"name":"movie.mp4", "source":"original", "format":"MPEG4"},
                {"name":"movie.mp3", "source":"derivative", "original":"movie.mp4"}
            ]
        }))
        .unwrap()
    }

    /// One exact metadata lookup restores a fresh History entry without replaying it.
    #[test]
    fn archive_now_playing_lookup_restores_fresh_history_video_without_catalogue_search() {
        let (_directory, mut controller, id) = playing_controller();
        let queue = controller.playback_queue.clone();
        let (entered, release, count) = super::super::tests::lookup_transport(&mut controller);
        assert!(controller.request_playing_archive_org(&id));
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(5)).unwrap().path(),
            "/metadata/playing"
        );
        assert_eq!(controller.view.screen, Screen::ArchiveOrg);
        assert!(
            controller.view.rows.is_empty(),
            "no fabricated loading track"
        );
        assert!(controller.request_playing_archive_org(&id));
        release.send(Ok(fresh_metadata())).unwrap();
        super::super::tests::finish_lookup_worker(&mut controller);
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(controller.view.selected, 1);
        assert_eq!(controller.view.rows.len(), 2);
        assert_eq!(
            controller.archive_org.active.as_ref().unwrap().tracks[1].filename,
            "movie.mp4"
        );
        assert_eq!(controller.playback_queue, queue);
        assert_eq!(controller.current_media, Some(id));
        assert!(controller.archive_org.pending.is_none());
    }

    /// Leaving and returning to a source, changing selection, or new playback revokes reveal.
    #[test]
    fn archive_now_playing_lookup_cannot_override_newer_navigation_or_playback() {
        for cancellation in ["tab", "selection", "playback", "back", "editing"] {
            let (_directory, mut controller, id) = playing_controller();
            for identifier in ["first", "second"] {
                let details = details(identifier);
                controller.archive_org.items.push(details.item.clone());
                controller
                    .archive_org
                    .cache
                    .push_back((identifier.into(), Ok(details)));
            }
            let (entered, release, count) = super::super::tests::lookup_transport(&mut controller);
            assert!(controller.request_playing_archive_org(&id));
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            match cancellation {
                "tab" => {
                    controller.show_screen(Screen::History);
                    controller.show_screen(Screen::ArchiveOrg);
                }
                "selection" => {
                    controller.view.selected = 1;
                    controller.update_archive_org_detail();
                    controller.view.selected = 0;
                    controller.update_archive_org_detail();
                }
                "playback" => {
                    controller.current_media =
                        Some(MediaId::new(SourceKind::YouTube, "abcdefghijk"))
                }
                "back" => assert!(controller.go_back_archive_org()),
                "editing" => controller.view.search_editing = true,
                _ => unreachable!(),
            }
            release.send(Ok(fresh_metadata())).unwrap();
            super::super::tests::finish_lookup_worker(&mut controller);
            assert!(controller.archive_org.active.is_none(), "{cancellation}");
            assert!(controller.cached_archive_details("playing").is_some());
            assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
        }
    }

    /// New metadata requests accept only canonical public files of the accepted track.
    #[test]
    fn archive_now_playing_lookup_rejects_unaccepted_or_unsafe_identity_without_work() {
        let (_directory, mut controller, accepted) = playing_controller();
        for source in [
            "https://archive.org/download/playing/movie.mp4?token=private",
            "https://user@archive.org/download/playing/movie.mp4",
            "https://archive.org/download/playing/a%2Fb.mp3",
            "https://archive.org/download/playing/a%00b.mp3",
            "https://elsewhere.example/download/playing/movie.mp4",
        ] {
            let id = MediaId::new(SourceKind::ArchiveOrg, source);
            controller.current_media = Some(id.clone());
            assert!(!controller.request_playing_archive_org(&id));
        }
        controller.current_media = None;
        assert!(!controller.request_playing_archive_org(&accepted));
        assert!(controller.archive_org.worker.is_none());
        assert!(controller.archive_org.pending.is_none());
        assert_eq!(controller.view.screen, Screen::History);
    }

    /// Missing exact files and failed metadata do not select a different file or retry.
    #[test]
    fn archive_now_playing_lookup_missing_or_failed_metadata_stays_truthful_and_bounded() {
        for failed in [false, true] {
            let (_directory, mut controller, id) = playing_controller();
            let (entered, release, count) = super::super::tests::lookup_transport(&mut controller);
            assert!(controller.request_playing_archive_org(&id));
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            release
                .send(if failed {
                    Err(crate::providers::ProviderError::HttpStatus(503))
                } else {
                    Ok(super::super::tests::lookup_metadata("playing"))
                })
                .unwrap();
            super::super::tests::finish_lookup_worker(&mut controller);
            for _ in 0..3 {
                controller.poll_archive_org_worker();
            }
            assert!(controller.archive_org.active.is_none());
            assert!(controller.archive_org.pending.is_none());
            assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
            assert!(controller.view.status_line.contains(if failed {
                "503"
            } else {
                "not listed"
            }));
            assert_eq!(controller.current_media, Some(id));
        }
    }

    /// Default and alternate encodings select the same family without changing its inventory.
    #[test]
    fn archive_now_playing_reveals_active_track_and_variant_without_playback() {
        let (_directory, mut controller) = super::super::tests::lookup_controller();
        let mut details = details("playing");
        for (filename, format) in [("original.mp4", "MPEG4"), ("original.webm", "WebM")] {
            Arc::make_mut(&mut details).tracks[0]
                .download_variants
                .push(ArchiveOrgDownloadVariant {
                    filename: filename.into(),
                    download_url: url::Url::parse(&format!(
                        "https://archive.org/download/playing/{filename}"
                    ))
                    .unwrap(),
                    format: format.into(),
                    size_bytes: None,
                    provenance: crate::providers::archive_org::ArchiveOrgFileProvenance::Original,
                    is_video: true,
                });
        }
        controller.archive_org.active = Some(Arc::clone(&details));
        let queue = controller.playback_queue.clone();
        for source in [
            &details.tracks[1].download_url,
            &details.tracks[0].download_variants[0].download_url,
            &details.tracks[0].download_variants[2].download_url,
            &details.tracks[0].download_variants[3].download_url,
        ] {
            let id = MediaId::new(SourceKind::ArchiveOrg, source.as_str());
            assert!(controller.reveal_playing_archive_org(&id));
            assert_eq!(controller.view.screen, Screen::ArchiveOrg);
            assert_eq!(controller.view.rows.len(), 2);
            assert_eq!(
                controller.view.selected,
                usize::from(source == &details.tracks[1].download_url)
            );
            assert!(Arc::ptr_eq(
                controller.archive_org.active.as_ref().unwrap(),
                &details
            ));
            assert_eq!(controller.playback_queue, queue);
            assert!(controller.view.playing_media_id.is_none());
            assert!(controller.archive_org.worker.is_none());
            assert!(controller.archive_org.history.is_empty());
        }
    }

    /// Cache and accepted autoplay metadata both restore Back navigation and exact identity.
    #[test]
    fn archive_now_playing_restores_cached_or_retained_item_and_previous_route() {
        for (retained, hidden) in [(false, false), (false, true), (true, false), (true, true)] {
            let (_directory, mut controller) = super::super::tests::lookup_controller();
            let old = details("browsing");
            let playing = details("playing");
            controller.view.screen = Screen::ArchiveOrg;
            controller.archive_org.items = vec![old.item.clone()];
            controller.archive_org.active = Some(Arc::clone(&old));
            controller.archive_org_selected = 1;
            controller.archive_org.submitted_query = "previous query".into();
            controller.archive_org_search_query = "previous query".into();
            controller.populate_archive_org();
            if hidden {
                controller.view.screen = Screen::History;
                controller.view.selected = 700;
            }
            if retained {
                controller.current_autoplay_origin = Some(AutoplayOrigin::ArchiveOrg {
                    details: Arc::clone(&playing),
                    index: 1,
                    preference: crate::config::ArchivePlaybackPreference::OriginalFile,
                });
            } else {
                controller
                    .archive_org
                    .cache
                    .push_back(("playing".into(), Ok(Arc::clone(&playing))));
            }
            let origin = controller.current_autoplay_origin.clone();
            assert!(controller.reveal_playing_archive_org(&track_id(&playing.tracks[1])));
            assert_eq!(controller.view.selected, 1);
            assert!(Arc::ptr_eq(
                controller.archive_org.active.as_ref().unwrap(),
                &playing
            ));
            assert_eq!(controller.archive_org.history.len(), 1);
            assert!(controller.go_back_archive_org());
            assert!(controller.archive_org.pending.is_none());
            assert!(controller.archive_org.worker.is_none());
            assert!(controller.go_back_archive_org());
            assert_eq!(
                controller
                    .archive_org
                    .active
                    .as_ref()
                    .unwrap()
                    .item
                    .identifier,
                "browsing"
            );
            assert_eq!(controller.view.selected, 1);
            assert_eq!(controller.archive_org.submitted_query, "previous query");
            assert_eq!(controller.current_autoplay_origin, origin);
            assert!(controller.archive_org.worker.is_none());
            assert!(controller.view.playing_media_id.is_none());
        }
    }

    /// A source reveal invalidates navigation responses but preserves pinned download ownership.
    #[test]
    fn archive_now_playing_revokes_old_open_without_canceling_manual_download_lookup() {
        for download in [false, true] {
            let (_directory, mut controller) = super::super::tests::lookup_controller();
            let playing = details("playing");
            controller.archive_org.active = Some(Arc::clone(&playing));
            let job = ArchiveJob {
                generation: 7,
                kind: ArchiveRequest::Details {
                    identifier: "other".into(),
                    open: !download,
                },
                due: Instant::now(),
            };
            controller.archive_org.generation = 7;
            controller.archive_org.pending = Some(job.clone());
            controller.archive_org.request = Some(job);
            if download {
                controller.archive_org.download_lookup = Some(ArchiveDownloadLookup {
                    source: url::Url::parse("https://archive.org/download/other/track.mp3")
                        .unwrap(),
                    identifier: "other".into(),
                    generation: 7,
                });
            }
            assert!(controller.reveal_playing_archive_org(&track_id(&playing.tracks[1])));
            assert_eq!(controller.archive_org.pending.is_some(), download);
            assert_eq!(controller.archive_org.request.is_some(), download);
            assert_eq!(controller.archive_org.download_lookup.is_some(), download);
            assert_eq!(controller.view.selected, 1);
            assert!(controller.archive_org.worker.is_none());
        }
    }

    /// Unknown or unsafe identities cannot replace the current Archive route.
    #[test]
    fn archive_now_playing_rejects_unknown_and_noncanonical_files() {
        let (_directory, mut controller) = super::super::tests::lookup_controller();
        controller.archive_org.active = Some(details("playing"));
        for source in [
            "https://archive.org/download/playing/missing.mp3",
            "https://archive.org/download/playing/second.mp3?token=private",
            "https://user@archive.org/download/playing/second.mp3",
            "http://archive.org/download/playing/second.mp3",
            "https://other.example/download/playing/second.mp3",
        ] {
            assert!(
                !controller
                    .reveal_playing_archive_org(&MediaId::new(SourceKind::ArchiveOrg, source))
            );
            assert_eq!(controller.view.screen, Screen::History);
            assert!(controller.archive_org.history.is_empty());
            assert!(controller.archive_org.worker.is_none());
        }
    }
}
