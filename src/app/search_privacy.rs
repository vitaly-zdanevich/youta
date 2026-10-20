//! One persistence policy for saved online searches and restart navigation.

use super::AppController;
#[cfg(any(test, feature = "apple-podcasts"))]
use crate::persistence::SavedApplePodcastsSearch;
#[cfg(any(test, feature = "bandcamp"))]
use crate::persistence::SavedBandcampSearch;
#[cfg(any(test, feature = "youtube-music"))]
use crate::persistence::SavedYouTubeMusicSearch;
use crate::persistence::{PersistenceError, SavedYouTubeSearch};

impl AppController {
    /// Switches automatic stores together without deleting prior activity.
    ///
    /// Cache operations already in progress finish before disabling returns;
    /// later jobs use private session paths. Failure leaves saving unchanged.
    ///
    /// # Errors
    /// Returns an error if private storage or its in-memory state cannot be prepared.
    pub(super) fn apply_activity_persistence(&mut self, enabled: bool) -> std::io::Result<()> {
        self.config.set_activity_persistence(enabled)?;
        if let Err(error) = self.store.set_activity_persistence(enabled) {
            let _ = self.config.set_activity_persistence(!enabled);
            return Err(std::io::Error::other(error));
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::persistence::StateStore;
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
        let (youtube, music, bandcamp, podcasts) = snapshots();
        for enabled in [false, true, false] {
            let directory = crate::test_support::canonical_tempdir("search persistence policy");
            let mut config = Config::for_dir(directory.path().join("config"));
            config.persistence.save_playback_history = enabled;
            let store = StateStore::open_in_memory().unwrap();
            let controller = AppController::new(config, store, None, None);
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
}
