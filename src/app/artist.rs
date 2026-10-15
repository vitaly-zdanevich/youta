//! Selection-owned artist navigation shared by music providers.

use super::*;

impl AppController {
    /// Refreshes only from the selected provider's retained exact artist identity.
    pub(super) fn refresh_selected_artist_availability(&mut self) {
        self.view.selected_artist_available = match self.view.screen {
            #[cfg(feature = "soundcloud")]
            Screen::SoundCloud => self.selected_soundcloud_artist_url().is_some(),
            #[cfg(feature = "funkwhale")]
            Screen::Funkwhale => self.can_open_funkwhale_artist(),
            #[cfg(feature = "yandex-music")]
            Screen::YandexMusic => self.selected_yandex_music_artist_id().is_some(),
            _ => false,
        };
    }

    /// Opens an exact selected artist without using the playing item or a name search.
    pub(super) fn open_selected_artist(&mut self) {
        match self.view.screen {
            #[cfg(feature = "soundcloud")]
            Screen::SoundCloud => {
                if let Some(url) = self.selected_soundcloud_artist_url() {
                    self.open_soundcloud_artist(url.to_string(), false);
                }
            }
            #[cfg(feature = "funkwhale")]
            Screen::Funkwhale => self.open_funkwhale_artist(),
            #[cfg(feature = "yandex-music")]
            Screen::YandexMusic => self.open_selected_yandex_music_artist(),
            _ => {}
        }
    }

    /// Returns the primary exact artist represented by a Yandex track or album.
    #[cfg(feature = "yandex-music")]
    fn selected_yandex_music_artist_id(&self) -> Option<&str> {
        let artists = self
            .yandex_music_rows
            .get(self.view.selected)
            .map(|row| match row {
                YandexMusicRow::Track(track) => track.artists.as_slice(),
                YandexMusicRow::Album(album) => album.artists.as_slice(),
            })?;
        artists.iter().find_map(|artist| artist.id.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stale Details and the playing item cannot expose an artist on another tab.
    #[test]
    fn artist_availability_cannot_escape_its_selected_provider() {
        let mut app = AppController::new(
            Config::for_dir("/tmp/youta-selected-artist-fixture"),
            StateStore::open_in_memory().unwrap(),
            None,
            None,
        );
        app.diagnostic_only = true;
        app.view.screen = Screen::Local;
        app.view.details = Some(DetailView {
            channel_id: "https://soundcloud.com/artist".to_owned(),
            ..DetailView::default()
        });
        app.view.selected_artist_available = true;
        app.refresh_selected_artist_availability();
        assert!(!app.view.selected_artist_available);
        app.dispatch(UiAction::OpenSelectedArtist);
        assert_eq!(app.view.screen, Screen::Local);
    }

    /// An unidentified selection cannot start a name-based search or a browser.
    #[test]
    fn missing_artist_identity_keeps_navigation_unchanged() {
        let mut app = AppController::new(
            Config::for_dir("/tmp/youta-missing-artist-fixture"),
            StateStore::open_in_memory().unwrap(),
            None,
            None,
        );
        app.diagnostic_only = true;
        for screen in [Screen::SoundCloud, Screen::Funkwhale, Screen::YandexMusic] {
            app.view.screen = screen;
            app.open_selected_artist();
            assert_eq!(app.view.screen, screen);
            assert!(!app.view.selected_artist_available);
        }
    }
}
