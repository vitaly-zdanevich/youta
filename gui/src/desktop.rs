//! The surfaces a desktop application has and a terminal does not.
//!
//! A menu bar, a tray icon, a notification, and a drop target are all ways of
//! reaching the same reducer from outside the page. None of them holds state:
//! every one of them either dispatches a [`UiAction`] or reads a published
//! [`ViewModel`], exactly as the window does.
//!
//! The menu is built from actions rather than from labels: a menu item's id
//! *is* its serialized action, so the entry is type-checked where it is
//! declared and there is no second table to drift out of step with the first.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::de::Error as _;
use tauri::menu::{AboutMetadata, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_notification::NotificationExt;

use youta::domain::MediaId;
use youta::view::{Screen, UiAction, ViewModel};

use crate::reducer::ReducerHandle;

/// Label of the one window declared in `tauri.conf.json`.
pub const MAIN_WINDOW: &str = "main";

/// Tray icon identifier, used to reach it again from the reducer thread.
const TRAY_ID: &str = "youta";

/// Menu id of the one entry that is not an action.
///
/// Showing a hidden window is a window-manager operation, not a change of
/// interactive state, so the reducer has no vocabulary for it and should not
/// grow one. The id is deliberately not valid JSON, so it can never be mistaken
/// for a serialized action.
const SHOW_WINDOW_ID: &str = "youta:show-window";

/// Longest provider text copied into a window title, tooltip, or notification.
///
/// A track title is provider text, and every other place Youta shows external
/// text bounds it. These are worse than most: an operating system decides how
/// to lay out an oversized title bar or notification, and the answers differ.
const MAX_ANNOUNCED_CHARS: usize = 120;

/// Whether the window currently has keyboard focus.
///
/// Shared with the reducer thread because it is the one thing a track-change
/// notification depends on that is not in the snapshot: a notification for
/// music the user is already watching play is noise.
#[derive(Clone, Debug)]
pub struct WindowFocus(Arc<AtomicBool>);

impl Default for WindowFocus {
    /// A window that has just opened has focus.
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
}

impl WindowFocus {
    /// Records a focus change reported by the window.
    pub fn set(&self, focused: bool) {
        self.0.store(focused, Ordering::Relaxed);
    }

    /// Reads the most recent focus state.
    #[must_use]
    pub fn focused(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What the desktop surfaces should say about the current snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Announcement {
    /// Window title.
    pub title: String,
    /// Tray tooltip.
    pub tooltip: String,
    /// Title and body of a notification, when a track changed unheard.
    pub notification: Option<(String, String)>,
}

/// What the desktop surfaces were last told.
#[derive(Debug, Default)]
pub struct Announced {
    /// Whether any snapshot has been observed yet.
    ///
    /// The first one seeds the comparison without notifying: a queue restored
    /// from durable state is not a track that started, and announcing it would
    /// greet the user with a notification for silence.
    started: bool,
    /// Identity of the entry playback was on.
    playing: Option<MediaId>,
    /// The title and tooltip already displayed.
    shown: Option<(String, String)>,
}

/// Trims provider text to something an operating system can lay out.
pub fn bounded(text: &str) -> String {
    text.chars().take(MAX_ANNOUNCED_CHARS).collect()
}

/// Returns what the desktop surfaces should say, or `None` when they say it.
///
/// Called for every changed snapshot, which during playback is several times a
/// second, so the comparison matters: re-setting a window title at that rate is
/// visible on some platforms and pointless on all of them.
pub fn announce(view: &ViewModel, focused: bool, state: &mut Announced) -> Option<Announcement> {
    let (title, tooltip, playing) = match view.now_playing.as_ref() {
        Some(playing) => {
            let track = bounded(&playing.title);
            let creator = bounded(&playing.subtitle);
            // An idle backend reports itself paused, because a backend holding
            // no media is not running. The queue can have a current entry
            // before anything has started, and calling that "Paused" would
            // claim the user stopped something they never began.
            let paused = view.playback.paused && !view.playback.idle;
            let tooltip = match (paused, creator.is_empty()) {
                (true, true) => format!("Paused · {track}"),
                (true, false) => format!("Paused · {track}\n{creator}"),
                (false, true) => track.clone(),
                (false, false) => format!("{track}\n{creator}"),
            };
            (
                format!("{track} — Youta"),
                tooltip,
                Some(playing.media_id.clone()),
            )
        }
        None => (
            "Youta".to_owned(),
            "Youta — nothing is playing".to_owned(),
            None,
        ),
    };

    let changed_track = state.started && state.playing != playing;
    let notification = (changed_track && !focused)
        .then(|| view.now_playing.as_ref())
        .flatten()
        .map(|playing| (bounded(&playing.title), bounded(&playing.subtitle)));

    state.started = true;
    state.playing = playing;
    if notification.is_none() && state.shown.as_ref() == Some(&(title.clone(), tooltip.clone())) {
        return None;
    }
    state.shown = Some((title.clone(), tooltip.clone()));
    Some(Announcement {
        title,
        tooltip,
        notification,
    })
}

/// Applies one announcement to the window title, the tray, and notifications.
///
/// Absent surfaces are skipped rather than reported: a tray icon is refused
/// outright by some Linux desktops, and a window that has already gone away
/// during shutdown is ordinary. A notification that fails, however, is
/// reported — it is the one of the three that the user was supposed to see.
pub fn perform<R: Runtime>(app: &AppHandle<R>, announcement: &Announcement) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    let _ = window.set_title(&announcement.title);
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(&announcement.tooltip));
    }
    if let Some((title, body)) = announcement.notification.as_ref()
        && let Err(error) = app.notification().builder().title(title).body(body).show()
    {
        eprintln!("the Youta window could not raise a notification: {error}");
    }
}

/// Builds one menu item whose identity is the action it dispatches.
fn item<R: Runtime>(
    app: &AppHandle<R>,
    action: &UiAction,
    text: &str,
) -> tauri::Result<MenuItem<R>> {
    let id = serde_json::to_string(action)?;
    // Encoding an action is very nearly infallible — `serde_json` writes a
    // non-finite float as `null` rather than refusing it — so the round trip is
    // asked for here, where the entry is declared. Without it a malformed
    // payload becomes an id that parses back to nothing, and the only symptom
    // is a menu entry that does nothing when it is clicked.
    if serde_json::from_str::<UiAction>(&id).ok().as_ref() != Some(action) {
        return Err(tauri::Error::Json(serde_json::Error::custom(format!(
            "the menu entry {text:?} does not survive its own id"
        ))));
    }
    // No accelerator, ever, on Youta's own entries. An accelerator is resolved
    // by the operating system before the page sees the key, whereas the shared
    // keymap resolves the same key against live modal state on the reducer
    // thread — so `Space` here would pause playback while the user typed a
    // space into the search field. The predefined items below keep their
    // platform accelerators because those belong to the web view's own text
    // editing, which is where Ctrl/Cmd+C and Cmd+Q come from.
    MenuItem::with_id(app, id, text, true, None::<&str>)
}

/// Returns the action a menu id names, if it names one.
///
/// Predefined items carry Tauri's own ids and the window entry carries a
/// deliberately unparseable one, so a failure here is the ordinary case for
/// them rather than an error.
fn action_of(id: &MenuId) -> Option<UiAction> {
    serde_json::from_str(id.as_ref()).ok()
}

/// Builds the application menu.
///
/// The Edit submenu is not decoration and must not be dropped: its predefined
/// items are what give the web view working cut, copy, paste, and select-all
/// keys, which is how the Details text selection introduced with the window is
/// actually copied. Replacing Tauri's default macOS menu without them would
/// take Cmd+C away silently.
pub fn menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let preferences = item(app, &UiAction::OpenPreferences, "Preferences…")?;
    let about = AboutMetadata {
        name: Some("Youta".to_owned()),
        version: Some(app.package_info().version.to_string()),
        ..AboutMetadata::default()
    };

    let file = Submenu::with_items(
        app,
        "File",
        true,
        &[
            &item(app, &UiAction::Download, "Download selected")?,
            &item(app, &UiAction::OpenInBrowser, "Open in browser")?,
            &item(app, &UiAction::CopyLink, "Copy link")?,
            &PredefinedMenuItem::separator(app)?,
            #[cfg(not(target_os = "macos"))]
            &preferences,
            #[cfg(not(target_os = "macos"))]
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None)?,
            #[cfg(not(target_os = "macos"))]
            &PredefinedMenuItem::quit(app, None)?,
        ],
    )?;

    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;

    // The steps repeat the terminal's exactly: two front-ends whose seek and
    // volume move by different amounts are two different players.
    let playback = Submenu::with_items(
        app,
        "Playback",
        true,
        &[
            &item(app, &UiAction::TogglePause, "Play / Pause")?,
            &PredefinedMenuItem::separator(app)?,
            &item(app, &UiAction::PlayQueueNeighbour(-1), "Previous track")?,
            &item(app, &UiAction::PlayQueueNeighbour(1), "Next track")?,
            &PredefinedMenuItem::separator(app)?,
            &item(app, &UiAction::SeekRelative(-5), "Back 5 seconds")?,
            &item(app, &UiAction::SeekRelative(5), "Forward 5 seconds")?,
            &item(app, &UiAction::ChangeChapter(-1), "Previous chapter")?,
            &item(app, &UiAction::ChangeChapter(1), "Next chapter")?,
            &PredefinedMenuItem::separator(app)?,
            &item(app, &UiAction::ChangeVolume(5), "Louder")?,
            &item(app, &UiAction::ChangeVolume(-5), "Quieter")?,
            &item(app, &UiAction::ChangeSpeed(0.1), "Faster")?,
            &item(app, &UiAction::ChangeSpeed(-0.1), "Slower")?,
            &PredefinedMenuItem::separator(app)?,
            &item(app, &UiAction::ToggleRepeat, "Repeat")?,
            &item(app, &UiAction::ToggleAutoplay, "Autoplay")?,
            &PredefinedMenuItem::separator(app)?,
            &item(app, &UiAction::PlayNext, "Queue selected item next")?,
            &item(app, &UiAction::AddToQueue, "Add selected item to queue")?,
            &item(app, &UiAction::OpenQueuePopup, "Show the queue")?,
        ],
    )?;

    // One entry per source this build actually contains, from the same
    // catalogue the tab strip is built from.
    let sources = Screen::ALL
        .into_iter()
        .filter(|screen| screen.enabled())
        .map(|screen| item(app, &UiAction::ShowScreen(screen), screen.label()))
        .collect::<tauri::Result<Vec<_>>>()?;
    let mut view_items = sources
        .iter()
        .map(|entry| entry as &dyn tauri::menu::IsMenuItem<R>)
        .collect::<Vec<_>>();
    let separator = PredefinedMenuItem::separator(app)?;
    let now_playing = item(app, &UiAction::ShowNowPlaying, "Show what is playing")?;
    let waveform = item(app, &UiAction::ToggleWaveform, "Waveform")?;
    view_items.push(&separator);
    view_items.push(&now_playing);
    view_items.push(&waveform);
    #[cfg(target_os = "macos")]
    let fullscreen = PredefinedMenuItem::fullscreen(app, None)?;
    #[cfg(target_os = "macos")]
    {
        view_items.push(&separator);
        view_items.push(&fullscreen);
    }
    let view = Submenu::with_items(app, "View", true, &view_items)?;

    let help = Submenu::with_items(
        app,
        "Help",
        true,
        &[
            &item(app, &UiAction::ToggleHelp, "Keyboard shortcuts")?,
            &item(app, &UiAction::OpenProjectHistory, "Recent changes")?,
            #[cfg(not(target_os = "macos"))]
            &PredefinedMenuItem::about(app, None, Some(about))?,
        ],
    )?;

    let window = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            #[cfg(target_os = "macos")]
            &PredefinedMenuItem::separator(app)?,
            #[cfg(target_os = "macos")]
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;

    Menu::with_items(
        app,
        &[
            #[cfg(target_os = "macos")]
            &Submenu::with_items(
                app,
                "Youta",
                true,
                &[
                    &PredefinedMenuItem::about(app, None, Some(about))?,
                    &PredefinedMenuItem::separator(app)?,
                    &preferences,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::services(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            &file,
            &edit,
            &playback,
            &view,
            &window,
            &help,
        ],
    )
}

/// Adds the tray icon, or explains why the desktop refused it.
///
/// The tray does not keep Youta alive: closing the window still ends the
/// process, releases the durable-state lock, and stops the player. A tray that
/// outlived its window would be a second, invisible way to hold that lock.
///
/// Its menu opens on an ordinary click on every platform rather than only on
/// the platform-conventional button, because the tray exists here to carry
/// controls and a hidden menu carries none.
pub fn install_tray<R: Runtime>(app: &AppHandle<R>) {
    let Some(icon) = app.default_window_icon().cloned() else {
        eprintln!("the Youta window has no icon to put in the tray");
        return;
    };
    let menu = || -> tauri::Result<Menu<R>> {
        Menu::with_items(
            app,
            &[
                &MenuItem::with_id(app, SHOW_WINDOW_ID, "Show Youta", true, None::<&str>)?,
                &PredefinedMenuItem::separator(app)?,
                // Every entry here has to mean something without a list in
                // sight, which is why the queue's neighbours appear rather than
                // `PlayNext`: that one queues whatever the cursor is on, and a
                // tray menu shows no cursor.
                &item(app, &UiAction::TogglePause, "Play / Pause")?,
                &item(app, &UiAction::PlayQueueNeighbour(-1), "Previous track")?,
                &item(app, &UiAction::PlayQueueNeighbour(1), "Next track")?,
                &item(app, &UiAction::OpenQueuePopup, "Show the queue")?,
                &PredefinedMenuItem::separator(app)?,
                // Quit goes through the reducer like any other quit, so the
                // player is stopped and durable state is flushed on the way out.
                &item(app, &UiAction::Quit, "Quit Youta")?,
            ],
        )
    }();
    let built = menu.and_then(|menu| {
        TrayIconBuilder::with_id(TRAY_ID)
            .icon(icon)
            .icon_as_template(true)
            .menu(&menu)
            .show_menu_on_left_click(true)
            .tooltip("Youta")
            .build(app)
    });
    if let Err(error) = built {
        // A tray is unavailable on desktops without a StatusNotifier host, and
        // that is not a reason to refuse to start.
        eprintln!("the Youta tray icon is unavailable: {error}");
    }
}

/// Routes one menu selection to the reducer, or to the window manager.
pub fn on_menu_event<R: Runtime>(app: &AppHandle<R>, event: &MenuEvent) {
    if event.id().as_ref() == SHOW_WINDOW_ID {
        show_window(app);
        return;
    }
    let Some(action) = action_of(event.id()) else {
        // A predefined item; the platform has already performed it.
        return;
    };
    if let Some(reducer) = app.try_state::<ReducerHandle>()
        && let Err(error) = reducer.dispatch(action)
    {
        eprintln!("the Youta menu could not reach the reducer: {error}");
    }
}

/// Brings the window back from behind, or from the dock.
pub fn show_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::{Announced, MAX_ANNOUNCED_CHARS, action_of, announce};

    use tauri::menu::MenuId;
    use youta::domain::{MediaId, SourceKind};
    use youta::view::{NowPlayingView, Screen, UiAction, ViewModel};

    /// Builds a snapshot playing one identified track.
    fn playing(id: &str, title: &str, subtitle: &str) -> ViewModel {
        ViewModel {
            now_playing: Some(NowPlayingView {
                media_id: MediaId::new(SourceKind::YouTube, id),
                title: title.to_owned(),
                subtitle: subtitle.to_owned(),
            }),
            ..ViewModel::default()
        }
    }

    /// A menu id must round-trip to the action the entry was declared with.
    ///
    /// This is the whole reason the id is the action: the value is checked by
    /// the compiler where the item is built, and this checks that nothing on
    /// the way back through the platform's menu changes it.
    #[test]
    fn a_menu_id_is_the_action_it_dispatches() {
        for action in [
            UiAction::TogglePause,
            UiAction::SeekRelative(-5),
            UiAction::ChangeSpeed(-0.1),
            UiAction::ShowScreen(Screen::Local),
            UiAction::Quit,
        ] {
            let id = MenuId::new(serde_json::to_string(&action).expect("encode the action"));
            assert_eq!(action_of(&id), Some(action));
        }
    }

    /// Ids that are not actions must be recognised as such rather than guessed.
    #[test]
    fn a_predefined_item_names_no_action() {
        for id in ["youta:show-window", "1234", "", "Quit"] {
            assert_eq!(action_of(&MenuId::new(id)), None, "{id}");
        }
    }

    /// The first snapshot seeds the comparison and must not notify.
    ///
    /// A queue is restored from durable state, so something is "playing" before
    /// the user has done anything at all; announcing it would greet a new
    /// window with a notification for silence.
    #[test]
    fn a_restored_queue_is_not_a_track_that_started() {
        let mut state = Announced::default();
        let first = announce(&playing("a", "First", "Creator"), false, &mut state)
            .expect("the title and tooltip are set from the first snapshot");
        assert_eq!(first.title, "First — Youta");
        assert_eq!(first.tooltip, "First\nCreator");
        assert!(
            first.notification.is_none(),
            "the first observation only seeds the comparison"
        );
    }

    /// A track that changes out of sight notifies; the same track never does.
    #[test]
    fn only_an_unheard_change_of_track_notifies() {
        let mut state = Announced::default();
        announce(&playing("a", "First", ""), false, &mut state);

        assert!(
            announce(&playing("a", "First", ""), false, &mut state).is_none(),
            "an unchanged snapshot asks for nothing at all"
        );

        let changed = announce(&playing("b", "Second", "Creator"), false, &mut state)
            .expect("a new track is announced");
        assert_eq!(
            changed.notification,
            Some(("Second".to_owned(), "Creator".to_owned()))
        );

        // The same change, watched, is not news.
        let mut watched = Announced::default();
        announce(&playing("a", "First", ""), true, &mut watched);
        let seen = announce(&playing("b", "Second", ""), true, &mut watched)
            .expect("the title still changes");
        assert!(
            seen.notification.is_none(),
            "a focused window is already showing the change"
        );
    }

    /// Only a backend that actually holds media may be called paused.
    ///
    /// `PlaybackStatus::default` is `idle` *and* `paused`, because a backend
    /// with nothing loaded is not running. A queue restored from durable state
    /// therefore has a current entry with exactly that status, and reading
    /// `paused` alone would announce that the user stopped something they had
    /// never started.
    #[test]
    fn a_queued_entry_is_not_paused_until_something_holds_it() {
        let mut state = Announced::default();
        let queued = announce(&playing("a", "First", ""), true, &mut state)
            .expect("the queued entry is announced");
        assert_eq!(queued.tooltip, "First");

        let mut held = playing("a", "First", "");
        held.playback.idle = false;
        held.playback.paused = true;
        let paused = announce(&held, true, &mut state).expect("holding it changes the tooltip");
        assert_eq!(paused.tooltip, "Paused · First");
    }

    /// Stopping restores the plain title and says so in the tray.
    #[test]
    fn silence_is_announced_as_silence() {
        let mut state = Announced::default();
        announce(&playing("a", "First", ""), true, &mut state);
        let stopped =
            announce(&ViewModel::default(), true, &mut state).expect("the title returns to plain");
        assert_eq!(stopped.title, "Youta");
        assert_eq!(stopped.tooltip, "Youta — nothing is playing");
        assert!(stopped.notification.is_none());
    }

    /// Provider text reaches an operating system here, so it is bounded.
    #[test]
    fn an_oversized_title_is_bounded_before_it_leaves_the_process() {
        let mut state = Announced::default();
        let announcement = announce(
            &playing("a", &"é".repeat(4096), &"ß".repeat(4096)),
            false,
            &mut state,
        )
        .expect("an announcement is produced");
        // The title carries the suffix as well, so the bound is on the part
        // that came from the provider.
        assert_eq!(
            announcement.title.chars().count(),
            MAX_ANNOUNCED_CHARS + " — Youta".chars().count()
        );
        assert_eq!(
            announcement.tooltip.chars().count(),
            MAX_ANNOUNCED_CHARS * 2 + 1
        );
    }
}
