//! Keeps automatic activity in memory while explicit user records remain durable.

use std::collections::HashMap;

use super::{
    CachedChannelSummary, CachedMetadata, CachedSubscriptionItems, CachedWikidataLookup,
    FileStateStore, HistoryEntry, ListenTotal, MediaId, PersistenceError, PlaybackProgress,
    SavedApplePodcastsSearch, SavedBandcampSearch, SavedYouTubeMusicSearch, SavedYouTubeSearch,
    SessionState, SourceKind, StateBackend, StateStore, VideoOrientation,
};

/// Defines activity writes that share the same process-local persistence boundary.
macro_rules! activity_write {
	($($name:ident($($argument:ident: $kind:ty),*) -> $result:ty;)*) => {
		$(
			#[doc = concat!("Applies [`StateBackend::", stringify!($name), "`] to the current activity store.")]
			///
			/// Changes remain in memory while activity persistence is disabled.
			///
			/// # Errors
			/// Returns the selected backend's validation or persistence error.
			pub fn $name(&self, $($argument: $kind),*) -> Result<$result, PersistenceError> {
				self.activity_backend().$name($($argument),*)
			}
		)*
	};
}

/// Defines lookups that retain access to older durable data behind the private overlay.
macro_rules! activity_lookup {
	($($name:ident($($argument:ident: $kind:ty),*) -> $result:ty;)*) => {
		$(
			#[doc = concat!("Reads [`StateBackend::", stringify!($name), "`] with private activity taking precedence.")]
			///
			/// # Errors
			/// Returns the selected backend's validation or persistence error.
			pub fn $name(&self, $($argument: $kind),*) -> Result<Option<$result>, PersistenceError> {
				if let Some(activity) = &self.activity
					&& let Some(value) = activity.$name($($argument),*)? {
					return Ok(Some(value));
				}
				self.backend.$name($($argument),*)
			}
		)*
	};
}

impl StateStore {
    /// Changes whether automatic activity is persisted without rewriting existing data.
    ///
    /// Disabling creates a fresh in-memory overlay. Re-enabling discards that
    /// overlay; private progress, queries, caches, and statistics are never flushed
    /// into the durable backend. Explicit notes, bookmarks, playlists, reactions,
    /// and download intentions continue to use the durable backend throughout.
    ///
    /// # Errors
    /// Returns an error if the empty in-memory backend cannot be initialized.
    pub fn set_activity_persistence(&mut self, enabled: bool) -> Result<(), PersistenceError> {
        if enabled {
            self.activity = None;
        } else if self.activity.is_none() {
            self.activity = Some(FileStateStore::open_in_memory()?);
        }
        Ok(())
    }

    /// Selects the only backend that may receive automatic activity mutations.
    fn activity_backend(&self) -> &dyn StateBackend {
        self.activity
            .as_ref()
            .map_or(self.backend.as_ref(), |activity| activity)
    }

    activity_write! {
        checkpoint_playback(progress: &PlaybackProgress, listen_source: &SourceKind, listened_seconds: u64) -> ();
        checkpoint_listening(source: &SourceKind, listened_seconds: u64) -> ();
        flush_playback_checkpoint() -> ();
        upsert_progress(progress: &PlaybackProgress) -> ();
        delete_progress(media_id: &MediaId) -> bool;
        save_session(state: &SessionState, updated_at: i64) -> ();
        save_youtube_search(search: &SavedYouTubeSearch, updated_at: i64) -> ();
        update_saved_youtube_video_orientation(video_id: &str, orientation: VideoOrientation, updated_at: i64) -> bool;
        clear_youtube_search() -> bool;
        save_youtube_music_search(search: &SavedYouTubeMusicSearch, updated_at: i64) -> ();
        clear_youtube_music_search() -> bool;
        save_bandcamp_search(search: &SavedBandcampSearch, updated_at: i64) -> ();
        clear_bandcamp_search() -> bool;
        save_apple_podcasts_search(search: &SavedApplePodcastsSearch, updated_at: i64) -> ();
        clear_apple_podcasts_search() -> bool;
        put_cached_subscription_items(cached: &CachedSubscriptionItems) -> ();
        delete_cached_subscription_items(source: &SourceKind, source_id: &str) -> bool;
        add_listen_seconds(source: &SourceKind, seconds: u64) -> ();
        reset_listen_seconds(source: &SourceKind) -> bool;
        put_cached_metadata(cached: &CachedMetadata) -> ();
        delete_expired_metadata(now: i64) -> usize;
        put_cached_channel_summary(cached: &CachedChannelSummary) -> ();
        delete_expired_channel_summaries(now: i64) -> usize;
        put_cached_wikidata(cached: &CachedWikidataLookup) -> ();
        delete_expired_wikidata(now: i64) -> usize;
    }

    activity_lookup! {
        progress(media_id: &MediaId) -> PlaybackProgress;
        session() -> SessionState;
        youtube_search() -> SavedYouTubeSearch;
        youtube_music_search() -> SavedYouTubeMusicSearch;
        bandcamp_search() -> SavedBandcampSearch;
        apple_podcasts_search() -> SavedApplePodcastsSearch;
        cached_subscription_items(source: &SourceKind, source_id: &str) -> CachedSubscriptionItems;
        cached_metadata(media_id: &MediaId) -> CachedMetadata;
        cached_channel_summary(channel_id: &str) -> CachedChannelSummary;
        cached_wikidata(property_id: &str, external_id: &str) -> CachedWikidataLookup;
    }

    /// Loads durable progress with process-local positions overriding matching identities.
    ///
    /// # Errors
    /// Returns the backend's bounded lookup or decoding error.
    pub fn progress_for_media_ids(
        &self,
        media_ids: &[MediaId],
    ) -> Result<HashMap<MediaId, PlaybackProgress>, PersistenceError> {
        let mut progress = self.backend.progress_for_media_ids(media_ids)?;
        if let Some(activity) = &self.activity {
            progress.extend(activity.progress_for_media_ids(media_ids)?);
        }
        Ok(progress)
    }

    /// Inserts a history record, using negative IDs for private in-memory entries.
    ///
    /// # Errors
    /// Returns a backend validation or persistence error.
    pub fn insert_history(&self, entry: &HistoryEntry) -> Result<i64, PersistenceError> {
        if let Some(activity) = &self.activity {
            return activity.insert_history(entry).map(|id| -id);
        }
        self.backend.insert_history(entry)
    }

    /// Updates a history entry without exposing a private ID to the durable backend.
    ///
    /// Existing durable entries remain unchanged while activity saving is disabled.
    ///
    /// # Errors
    /// Returns a backend validation or persistence error.
    pub fn update_history(&self, entry: &HistoryEntry) -> Result<bool, PersistenceError> {
        if let Some(activity) = &self.activity {
            if entry.id >= 0 {
                return Ok(false);
            }
            let mut private = entry.clone();
            let Some(id) = private.id.checked_neg() else {
                return Ok(false);
            };
            private.id = id;
            return activity.update_history(&private);
        }
        if entry.id < 0 {
            return Ok(false);
        }
        self.backend.update_history(entry)
    }

    /// Lists previous durable history together with any process-local entries.
    ///
    /// # Errors
    /// Returns a backend lookup or decoding error.
    pub fn history(
        &self,
        finished_only: bool,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>, PersistenceError> {
        let mut entries = self.backend.history(finished_only, limit)?;
        if let Some(activity) = &self.activity {
            entries.extend(
                activity
                    .history(finished_only, limit)?
                    .into_iter()
                    .map(|mut entry| {
                        entry.id = -entry.id;
                        entry
                    }),
            );
            entries.sort_by(|left, right| {
                right
                    .last_played_at
                    .cmp(&left.last_played_at)
                    .then_with(|| right.id.cmp(&left.id))
            });
            entries.truncate(limit);
        }
        Ok(entries)
    }

    /// Removes a history row only from the currently writable activity store.
    ///
    /// # Errors
    /// Returns a backend persistence error.
    pub fn delete_history(&self, id: i64) -> Result<bool, PersistenceError> {
        if let Some(activity) = &self.activity {
            return match id.checked_neg().filter(|_| id < 0) {
                Some(id) => activity.delete_history(id),
                None => Ok(false),
            };
        }
        if id < 0 {
            return Ok(false);
        }
        self.backend.delete_history(id)
    }

    /// Returns existing listening time plus the current private session's delta.
    ///
    /// # Errors
    /// Returns a backend lookup error.
    pub fn listened_seconds(&self, source: &SourceKind) -> Result<u64, PersistenceError> {
        let durable = self.backend.listened_seconds(source)?;
        let private = self
            .activity
            .as_ref()
            .map(|activity| activity.listened_seconds(source))
            .transpose()?
            .unwrap_or(0);
        Ok(durable.saturating_add(private))
    }

    /// Lists listening totals without persisting the current private session's time.
    ///
    /// # Errors
    /// Returns a backend lookup error.
    pub fn listen_totals(&self) -> Result<Vec<ListenTotal>, PersistenceError> {
        let mut totals = self.backend.listen_totals()?;
        if let Some(activity) = &self.activity {
            for private in activity.listen_totals()? {
                if let Some(total) = totals
                    .iter_mut()
                    .find(|total| total.source == private.source)
                {
                    total.total_seconds = total.total_seconds.saturating_add(private.total_seconds);
                } else {
                    totals.push(private);
                }
            }
            totals.sort_by(|left, right| {
                right
                    .total_seconds
                    .cmp(&left.total_seconds)
                    .then_with(|| left.source.as_str().cmp(right.source.as_str()))
            });
        }
        Ok(totals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, PersistenceBackend};
    use crate::domain::{Bookmark, CommentTarget};
    use crate::providers::{SearchRequest, SearchTarget};

    /// Reads the activity documents whose bytes must remain unchanged in private mode.
    fn activity_bytes(config: &Config) -> Vec<Vec<u8>> {
        [
            config.state_dir().join("progress.toml"),
            config.state_dir().join("history.toml"),
            config.state_dir().join("statistics.toml"),
            config.runtime_dir().join("session.toml"),
            config.runtime_dir().join("playback-checkpoint.toml"),
            config.cache_dir().join("searches.toml"),
            config.cache_dir().join("providers.toml"),
        ]
        .into_iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect()
    }

    /// Disabling activity persistence preserves previous bytes and explicit playlists.
    #[test]
    fn disabled_activity_keeps_new_playback_and_searches_off_disk() {
        let directory = crate::test_support::canonical_tempdir("private activity persistence");
        let mut config = Config::for_dir(directory.path().join("config"));
        drop(StateStore::open(&config).unwrap());
        let before = activity_bytes(&config);
        config.persistence.save_playback_history = false;
        {
            let store = StateStore::open(&config).unwrap();
            let mut progress = PlaybackProgress::new(
                MediaId::new(SourceKind::YouTube, "private-video"),
                Some(100),
                1,
            );
            progress.record_position(25, 2);
            store
                .checkpoint_playback(&progress, &SourceKind::YouTube, 12)
                .unwrap();
            store.flush_playback_checkpoint().unwrap();
            store
                .save_session(
                    &SessionState {
                        selected_media: Some(progress.media_id.clone()),
                        search_text: "private query".to_owned(),
                        ..SessionState::default()
                    },
                    2,
                )
                .unwrap();
            store
                .save_youtube_search(
                    &SavedYouTubeSearch {
                        request: SearchRequest::new("private query", SearchTarget::Videos),
                        results: Vec::new(),
                        next_page: None,
                    },
                    2,
                )
                .unwrap();
            store.create_playlist("Explicit playlist", None, 2).unwrap();
            assert_eq!(store.progress(&progress.media_id).unwrap(), Some(progress));
        }
        assert_eq!(activity_bytes(&config), before);
        let reopened = StateStore::open(&config).unwrap();
        assert!(
            reopened
                .progress(&MediaId::new(SourceKind::YouTube, "private-video"))
                .unwrap()
                .is_none()
        );
        assert_eq!(reopened.playlists().unwrap()[0].name, "Explicit playlist");
    }

    /// Models an existing durable record, private changes, and later enabled writes.
    fn assert_private_overlay_is_discarded(backend: PersistenceBackend) {
        let directory = crate::test_support::canonical_tempdir("private overlay transition");
        let mut config = Config::for_dir(directory.path().join("config"));
        config.persistence.backend = backend;
        let media_id = MediaId::new(SourceKind::YouTube, "existing-video");
        let progress = PlaybackProgress::new(media_id.clone(), Some(100), 1);
        let old_search = SavedYouTubeSearch {
            request: SearchRequest::new("previous query", SearchTarget::Videos),
            results: Vec::new(),
            next_page: None,
        };
        let old_lookup = CachedWikidataLookup {
            property_id: "P1651".to_owned(),
            external_id: "existing-video".to_owned(),
            items: Vec::new(),
            fetched_at: 1,
            expires_at: 100,
        };
        let old_session = SessionState {
            local_path: Some("/existing".to_owned()),
            ..SessionState::default()
        };
        let mut store = StateStore::open(&config).unwrap();
        store.upsert_progress(&progress).unwrap();
        store.add_listen_seconds(&SourceKind::YouTube, 9).unwrap();
        store.save_youtube_search(&old_search, 1).unwrap();
        store.put_cached_wikidata(&old_lookup).unwrap();
        store.save_session(&old_session, 1).unwrap();
        let old_history = HistoryEntry {
            id: 0,
            media_id: media_id.clone(),
            title: "Previous item".to_owned(),
            replay_locator: None,
            started_at: 1,
            last_played_at: 1,
            position_seconds: 0,
            duration_seconds: Some(100),
            finished: false,
        };
        let old_id = store.insert_history(&old_history).unwrap();
        store.set_activity_persistence(false).unwrap();
        assert_eq!(store.progress(&media_id).unwrap(), Some(progress.clone()));
        let mut private_progress = progress.clone();
        private_progress.record_position(75, 2);
        store
            .checkpoint_playback(&private_progress, &SourceKind::YouTube, 40)
            .unwrap();
        let private_lookup = CachedWikidataLookup {
            external_id: "private-video".to_owned(),
            ..old_lookup.clone()
        };
        store.put_cached_wikidata(&private_lookup).unwrap();
        store
            .save_session(
                &SessionState {
                    local_path: Some("/private".to_owned()),
                    ..old_session.clone()
                },
                2,
            )
            .unwrap();
        store
            .save_youtube_search(
                &SavedYouTubeSearch {
                    request: SearchRequest::new("private query", SearchTarget::Videos),
                    ..old_search.clone()
                },
                2,
            )
            .unwrap();
        let mut private_history = HistoryEntry {
            title: "Private item".to_owned(),
            ..old_history.clone()
        };
        private_history.id = store.insert_history(&private_history).unwrap();
        private_history.position_seconds = 75;
        assert!(store.update_history(&private_history).unwrap());
        assert!(!store.delete_history(old_id).unwrap());
        assert_eq!(
            store.progress(&media_id).unwrap(),
            Some(private_progress.clone())
        );
        assert_eq!(
            store
                .progress_for_media_ids(std::slice::from_ref(&media_id))
                .unwrap()[&media_id],
            private_progress
        );
        assert_eq!(store.listened_seconds(&SourceKind::YouTube).unwrap(), 49);
        let target = CommentTarget::Media {
            media_id: media_id.clone(),
        };
        store
            .upsert_private_note(&target, "Explicit note", 2)
            .unwrap();
        store
            .insert_bookmark(&Bookmark {
                id: 0,
                media_id: media_id.clone(),
                position_seconds: 20,
                label: Some("Explicit bookmark".to_owned()),
                created_at: 2,
            })
            .unwrap();
        let queue = crate::download_queue::tests::fixture();
        store.save_download_queue(&queue).unwrap();
        store.set_activity_persistence(true).unwrap();
        store.flush_playback_checkpoint().unwrap();
        assert_eq!(store.progress(&media_id).unwrap(), Some(progress.clone()));
        assert_eq!(store.listened_seconds(&SourceKind::YouTube).unwrap(), 9);
        assert_eq!(store.session().unwrap(), Some(old_session.clone()));
        assert_eq!(store.youtube_search().unwrap(), Some(old_search.clone()));
        assert!(
            store
                .cached_wikidata("P1651", "private-video")
                .unwrap()
                .is_none()
        );
        assert!(!store.update_history(&private_history).unwrap());
        assert_eq!(store.history(false, 10).unwrap().len(), 1);
        store
            .put_cached_wikidata(&CachedWikidataLookup {
                external_id: "later-public-video".to_owned(),
                ..old_lookup.clone()
            })
            .unwrap();
        drop(store);
        let store = StateStore::open(&config).unwrap();
        assert_eq!(store.progress(&media_id).unwrap(), Some(progress));
        assert_eq!(store.session().unwrap(), Some(old_session));
        assert_eq!(store.youtube_search().unwrap(), Some(old_search));
        assert_eq!(
            store.cached_wikidata("P1651", "existing-video").unwrap(),
            Some(old_lookup)
        );
        assert!(
            store
                .cached_wikidata("P1651", "private-video")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .cached_wikidata("P1651", "later-public-video")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.private_note(&target).unwrap().unwrap().body,
            "Explicit note"
        );
        assert_eq!(store.bookmarks(&media_id).unwrap().len(), 1);
        assert_eq!(store.download_queue().unwrap(), queue);
    }

    /// Re-enabling file persistence discards private state before subsequent rewrites.
    #[test]
    fn file_private_overlay_never_flushes_when_reenabled() {
        assert_private_overlay_is_discarded(PersistenceBackend::Files);
    }

    /// The SQLite backend preserves the same automatic/manual persistence boundary.
    #[cfg(feature = "sqlite-state")]
    #[test]
    fn sqlite_private_overlay_never_flushes_when_reenabled() {
        assert_private_overlay_is_discarded(PersistenceBackend::Sqlite);
    }

    /// Startup while disabled neither quarantines corrupt caches nor rewrites old checkpoints.
    #[test]
    fn private_startup_preserves_corrupt_cache_bytes_and_pending_checkpoint() {
        let directory = crate::test_support::canonical_tempdir("private startup recovery");
        let mut config = Config::for_dir(directory.path().join("config"));
        {
            let store = StateStore::open(&config).unwrap();
            let progress = PlaybackProgress::new(
                MediaId::new(SourceKind::YouTube, "old-pending"),
                Some(100),
                1,
            );
            store
                .checkpoint_playback(&progress, &SourceKind::YouTube, 12)
                .unwrap();
        }
        for path in [
            config.cache_dir().join("providers.toml"),
            config.cache_dir().join("searches.toml"),
            config.runtime_dir().join("session.toml"),
        ] {
            std::fs::write(path, b"old damaged document = [").unwrap();
        }
        let before = activity_bytes(&config);
        config.persistence.save_playback_history = false;
        drop(StateStore::open(&config).unwrap());
        assert_eq!(activity_bytes(&config), before);
        for directory in [config.cache_dir(), config.runtime_dir()] {
            assert_eq!(
                std::fs::read_dir(directory).unwrap().count(),
                2,
                "startup must not create quarantine copies"
            );
        }
    }

    /// New private installations create no restart snapshots or provider caches.
    #[test]
    fn private_first_open_does_not_create_runtime_or_cache_documents() {
        let directory = crate::test_support::canonical_tempdir("fresh private state");
        let mut config = Config::for_dir(directory.path().join("config"));
        config.persistence.save_playback_history = false;
        drop(StateStore::open(&config).unwrap());
        assert_eq!(std::fs::read_dir(config.cache_dir()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(config.runtime_dir()).unwrap().count(), 0);
        config.persistence.save_playback_history = true;
        let store = StateStore::open(&config).unwrap();
        store.save_session(&SessionState::default(), 1).unwrap();
        assert!(config.runtime_dir().join("session.toml").exists());
    }
}
