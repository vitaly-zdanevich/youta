//! One persistence policy for saved online searches and restart navigation.

use super::{AppController, unix_time};
#[cfg(any(test, feature = "apple-podcasts"))]
use crate::persistence::SavedApplePodcastsSearch;
#[cfg(any(test, feature = "bandcamp"))]
use crate::persistence::SavedBandcampSearch;
#[cfg(any(test, feature = "youtube-music"))]
use crate::persistence::SavedYouTubeMusicSearch;
use crate::persistence::{PersistenceError, SavedYouTubeSearch, StateStore};

impl AppController {
    /// Saves a `YouTube` snapshot only while online history persistence is enabled.
    ///
    /// # Errors
    /// Propagates validation and storage failures from an enabled write.
    pub(super) fn save_youtube_search(
        &self,
        search: &SavedYouTubeSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        if self.config.persistence.save_playback_history {
            self.store.save_youtube_search(search, updated_at)
        } else {
            Ok(())
        }
    }

    /// Saves a `YouTube Music` snapshot only while online history persistence is enabled.
    ///
    /// # Errors
    /// Propagates validation and storage failures from an enabled write.
    #[cfg(any(test, feature = "youtube-music"))]
    pub(super) fn save_youtube_music_search(
        &self,
        search: &SavedYouTubeMusicSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        if self.config.persistence.save_playback_history {
            self.store.save_youtube_music_search(search, updated_at)
        } else {
            Ok(())
        }
    }

    /// Saves a Bandcamp snapshot only while online history persistence is enabled.
    ///
    /// # Errors
    /// Propagates validation and storage failures from an enabled write.
    #[cfg(any(test, feature = "bandcamp"))]
    pub(super) fn save_bandcamp_search(
        &self,
        search: &SavedBandcampSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        if self.config.persistence.save_playback_history {
            self.store.save_bandcamp_search(search, updated_at)
        } else {
            Ok(())
        }
    }

    /// Saves an Apple Podcasts snapshot only while online history persistence is enabled.
    ///
    /// # Errors
    /// Propagates validation and storage failures from an enabled write.
    #[cfg(any(test, feature = "apple-podcasts"))]
    pub(super) fn save_apple_podcasts_search(
        &self,
        search: &SavedApplePodcastsSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        if self.config.persistence.save_playback_history {
            self.store.save_apple_podcasts_search(search, updated_at)
        } else {
            Ok(())
        }
    }
}

/// Removes old online searches and sanitizes only the already-persisted session.
///
/// Every provider slot is cleared even when its adapter is absent from this
/// build. Existing Local navigation and display preferences stay in the saved
/// session, which is replaced only when sanitization changes it. Live browsing
/// state, playback progress, statistics, and user-maintained collections are
/// untouched.
///
/// # Errors
/// Returns the first storage error after attempting all independent cleanup.
pub(super) fn clear_saved_online_searches(store: &StateStore) -> Result<(), PersistenceError> {
    let mut failure = None;
    for result in [
        store.clear_youtube_search(),
        store.clear_youtube_music_search(),
        store.clear_bandcamp_search(),
        store.clear_apple_podcasts_search(),
    ] {
        if let Err(error) = result {
            failure.get_or_insert(error);
        }
    }
    let session_result = (|| {
        if let Some(mut saved) = store.session()? {
            let previous = saved.clone();
            saved.clear_online_browsing();
            if saved != previous {
                store.save_session(&saved, unix_time())?;
            }
        }
        Ok::<_, PersistenceError>(())
    })();
    if let Err(error) = session_result {
        failure.get_or_insert(error);
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::{MediaId, PanelFocus, Screen, SessionState, SourceKind};
    use crate::providers::{SearchRequest, SearchTarget};

    /// Supplies independent valid queries without any network-dependent result metadata.
    fn snapshots() -> (
        SavedYouTubeSearch,
        SavedYouTubeMusicSearch,
        SavedBandcampSearch,
        SavedApplePodcastsSearch,
    ) {
        (
            SavedYouTubeSearch {
                request: SearchRequest::new("private youtube query", SearchTarget::Videos),
                results: Vec::new(),
                next_page: None,
            },
            SavedYouTubeMusicSearch {
                query: "private music query".to_owned(),
                results: Vec::new(),
            },
            SavedBandcampSearch {
                query: "private bandcamp query".to_owned(),
                page: 1,
                results: Vec::new(),
                next_page: None,
            },
            SavedApplePodcastsSearch {
                query: "private podcast query".to_owned(),
                storefront: "us".to_owned(),
                results: Vec::new(),
            },
        )
    }

    /// All provider writers honor the current setting, including responses received after a toggle.
    #[test]
    fn saved_search_writers_follow_the_current_history_setting() {
        let directory = crate::test_support::canonical_tempdir("search persistence policy");
        let config = Config::for_dir(directory.path().join("config"));
        let store = StateStore::open_in_memory().unwrap();
        let mut controller = AppController::new(config, store, None, None);
        let (youtube, music, bandcamp, podcasts) = snapshots();
        for enabled in [false, true, false] {
            controller.config.persistence.save_playback_history = enabled;
            clear_saved_online_searches(&controller.store).unwrap();
            controller.save_youtube_search(&youtube, 1).unwrap();
            controller.save_youtube_music_search(&music, 1).unwrap();
            controller.save_bandcamp_search(&bandcamp, 1).unwrap();
            controller.save_apple_podcasts_search(&podcasts, 1).unwrap();
            assert_eq!(
                controller.store.youtube_search().unwrap(),
                enabled.then(|| youtube.clone())
            );
            assert_eq!(
                controller.store.youtube_music_search().unwrap(),
                enabled.then(|| music.clone())
            );
            assert_eq!(
                controller.store.bandcamp_search().unwrap(),
                enabled.then(|| bandcamp.clone())
            );
            assert_eq!(
                controller.store.apple_podcasts_search().unwrap(),
                enabled.then(|| podcasts.clone())
            );
        }
    }

    /// Enabled persistence retains the backend's validation failures instead of suppressing them.
    #[test]
    fn enabled_search_writers_propagate_validation_errors() {
        let directory = crate::test_support::canonical_tempdir("search validation policy");
        let config = Config::for_dir(directory.path().join("config"));
        let store = StateStore::open_in_memory().unwrap();
        let controller = AppController::new(config, store, None, None);
        let (mut youtube, mut music, mut bandcamp, mut podcasts) = snapshots();
        youtube.request.query.clear();
        music.query.clear();
        bandcamp.query.clear();
        podcasts.query.clear();
        assert!(controller.save_youtube_search(&youtube, 1).is_err());
        assert!(controller.save_youtube_music_search(&music, 1).is_err());
        assert!(controller.save_bandcamp_search(&bandcamp, 1).is_err());
        assert!(controller.save_apple_podcasts_search(&podcasts, 1).is_err());
    }

    /// Cleanup removes every search slot and query while preserving Local navigation and statistics.
    #[test]
    fn cleanup_removes_saved_queries_without_losing_local_session_or_statistics() {
        let store = StateStore::open_in_memory().unwrap();
        let (youtube, music, bandcamp, podcasts) = snapshots();
        store.save_youtube_search(&youtube, 1).unwrap();
        store.save_youtube_music_search(&music, 1).unwrap();
        store.save_bandcamp_search(&bandcamp, 1).unwrap();
        store.save_apple_podcasts_search(&podcasts, 1).unwrap();
        store.add_listen_seconds(&SourceKind::YouTube, 37).unwrap();
        let local_identity = MediaId::new(SourceKind::Local, "/tmp/local fixture.flac");
        let original = SessionState {
            screen: Screen::Local,
            focus: PanelFocus::Right,
            selected_media: Some(local_identity.clone()),
            selected_row: 9,
            details_scroll: 12,
            search_text: youtube.request.query,
            youtube_music_search_text: music.query,
            soundcloud_search_text: "private soundcloud query".to_owned(),
            yandex_music_search_text: "private yandex query".to_owned(),
            bandcamp_search_text: bandcamp.query,
            apple_podcasts_search_text: podcasts.query,
            archive_org_search_text: "private archive query".to_owned(),
            librivox_search_text: "private librivox query".to_owned(),
            radio_filter_text: "private radio filter".to_owned(),
            local_path: Some("/tmp/local fixture".to_owned()),
            waveform_visible: true,
            chapter_timestamps_hidden: false,
            ..SessionState::default()
        };
        store.save_session(&original, 1).unwrap();
        clear_saved_online_searches(&store).unwrap();
        assert!(store.youtube_search().unwrap().is_none());
        assert!(store.youtube_music_search().unwrap().is_none());
        assert!(store.bandcamp_search().unwrap().is_none());
        assert!(store.apple_podcasts_search().unwrap().is_none());
        let saved = store.session().unwrap().unwrap();
        assert!(!serde_json::to_string(&saved).unwrap().contains("private"));
        assert_eq!(saved.screen, Screen::Local);
        assert_eq!(saved.local_path, original.local_path);
        assert_eq!(saved.selected_media, Some(local_identity));
        assert_eq!(saved.selected_row, 9);
        assert_eq!(saved.details_scroll, 12);
        assert_eq!(saved.focus, PanelFocus::Right);
        assert!(saved.waveform_visible);
        assert!(!saved.chapter_timestamps_hidden);
        assert_eq!(store.listened_seconds(&SourceKind::YouTube).unwrap(), 37);
        clear_saved_online_searches(&store).unwrap();
        assert_eq!(store.session().unwrap(), Some(saved));
    }

    /// Disabling history on a fresh store does not invent a persisted session.
    #[test]
    fn cleanup_of_empty_store_preserves_absent_session() {
        let store = StateStore::open_in_memory().unwrap();
        clear_saved_online_searches(&store).unwrap();
        assert!(store.session().unwrap().is_none());
    }

    /// A failed search-cache replacement must not prevent sanitizing the independent session.
    #[test]
    fn cleanup_reports_search_write_failure_and_still_sanitizes_session() {
        let directory = crate::test_support::canonical_tempdir("blocked search cleanup");
        let config = Config::for_dir(directory.path().join("config"));
        let store = StateStore::open(&config).unwrap();
        let (youtube, _, _, _) = snapshots();
        store.save_youtube_search(&youtube, 1).unwrap();
        store
            .save_session(
                &SessionState {
                    search_text: "private saved session query".to_owned(),
                    ..SessionState::default()
                },
                1,
            )
            .unwrap();
        let path = config.cache_dir().join("searches.toml");
        std::fs::rename(&path, directory.path().join("original-searches.toml")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(clear_saved_online_searches(&store).is_err());
        assert!(store.session().unwrap().unwrap().search_text.is_empty());
        assert!(
            !std::fs::read_to_string(config.runtime_dir().join("session.toml"))
                .unwrap()
                .contains("private saved session query")
        );
    }

    /// Repeated cleanup never attempts to rewrite a session that already obeys the policy.
    #[test]
    fn cleanup_does_not_rewrite_an_unchanged_session() {
        let directory = crate::test_support::canonical_tempdir("unchanged private session");
        let config = Config::for_dir(directory.path().join("config"));
        let store = StateStore::open(&config).unwrap();
        store.save_session(&SessionState::default(), 1).unwrap();
        let path = config.runtime_dir().join("session.toml");
        std::fs::rename(&path, directory.path().join("original-session.toml")).unwrap();
        std::fs::create_dir(&path).unwrap();
        clear_saved_online_searches(&store).unwrap();
        assert_eq!(store.session().unwrap(), Some(SessionState::default()));
    }
}
