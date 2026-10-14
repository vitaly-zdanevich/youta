//! Restart compatibility when a distributor removes the optional Funkwhale tab.

#![cfg(all(feature = "controller", not(feature = "funkwhale")))]

use youta::app::AppController;
use youta::config::Config;
use youta::domain::{Screen as StoredScreen, SessionState};
use youta::persistence::StateStore;
use youta::view::{Screen, UiController};

/// A disabled provider's old draft must never appear as a YouTube query.
#[test]
fn disabled_funkwhale_restart_uses_the_youtube_fallback_query() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open_in_memory().unwrap();
    store
        .save_session(
            &SessionState {
                screen: StoredScreen::Funkwhale,
                search_text: "youtube query".to_owned(),
                funkwhale_search_text: "pod query".to_owned(),
                ..SessionState::default()
            },
            1,
        )
        .unwrap();
    let app = AppController::new(Config::for_dir(directory.path()), store, None, None);
    assert_eq!(app.view().screen, Screen::Search);
    assert_eq!(app.view().search_query, "youtube query");
    assert!(!Screen::Funkwhale.enabled());
}
