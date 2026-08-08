//! The single-threaded reducer, hosted for an out-of-process front-end.
//!
//! [`youta::app::AppController`] is the only mutator of interactive state, and
//! it is deliberately not `Send`: it owns `Box<dyn StateBackend>`,
//! `Box<dyn PlaybackBackend>`, and a diagnostic handler, none of which the
//! shared crate requires to be thread-safe. The controller is therefore
//! *constructed on* its thread rather than moved onto one, which keeps the
//! window from forcing `Send` bounds onto three public traits that the terminal
//! front-end has no reason to carry.
//!
//! The window talks to that thread through a channel. This is the same
//! discipline the terminal event loop follows, with the renderer replaced by a
//! serialized snapshot.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

// The runtime is a type parameter so the reducer can be driven by Tauri's mock
// runtime in tests. Nothing here depends on the real windowing backend.
use tauri::{AppHandle, Emitter, Runtime};

use youta::app::AppController;
use youta::config::Config;
use youta::persistence::{ANOTHER_INSTANCE_MESSAGE, PersistenceError, StateStore};
use youta::playback::configured_playback_factory;
use youta::providers::configured_youtube_provider;
use youta::view::{UiAction, UiController, ViewModel};

/// Event name carrying a changed snapshot to the window.
pub const VIEW_EVENT: &str = "youta://view";

/// Reducer wake-up period.
///
/// The reducer also wakes immediately for every dispatched action, so this only
/// bounds how long a worker response waits before reaching the window.
const TICK: Duration = Duration::from_millis(100);

/// Actions applied before the next snapshot is published.
const MAX_ACTIONS_PER_TICK: usize = 64;

/// How long [`ReducerHandle::shutdown`] waits for durable state to close.
///
/// Exiting early would leave the player process running and the state lock
/// held, so the window waits. The bound exists only so a wedged worker cannot
/// keep a windowless process alive forever.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// One item on the reducer's inbox.
enum Message {
    /// A semantic action from the window.
    Action(UiAction),
    /// Stop the engine, flush durable state, and release the state lock.
    Stop,
}

/// Handle used by IPC commands to reach the reducer thread.
pub struct ReducerHandle {
    actions: Sender<Message>,
    latest: Arc<Mutex<ViewModel>>,
    /// A `Receiver` is `Send` but not `Sync`, and Tauri shares managed state
    /// across threads, so the exit path takes this lock to wait on it.
    finished: Mutex<Receiver<()>>,
}

impl ReducerHandle {
    /// Queues one semantic action for the reducer.
    ///
    /// # Errors
    ///
    /// Returns a message when the reducer thread is no longer running.
    pub fn dispatch(&self, action: UiAction) -> Result<(), String> {
        self.actions
            .send(Message::Action(action))
            .map_err(|_| "the Youta reducer stopped".to_owned())
    }

    /// Returns the most recently published snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ViewModel {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stops playback and durable state before the process ends.
    ///
    /// Closing the window is not a quit action inside the reducer, so nothing
    /// else would reach the shutdown path: the player process would outlive the
    /// window and the state lock would be released only by process death.
    pub fn shutdown(&self) {
        let _ = self.actions.send(Message::Stop);
        // `RecvTimeoutError` of either kind means the reducer is gone: it either
        // finished and dropped its sender, or it is wedged past the grace period.
        let _ = self
            .finished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(SHUTDOWN_GRACE);
    }
}

/// Starts the reducer thread and waits for it to finish opening state.
///
/// # Errors
///
/// Returns the startup failure text when durable state cannot be opened, which
/// includes the case of a second Youta process already holding the state lock.
pub fn start<R: Runtime>(app: AppHandle<R>, config: Config) -> Result<ReducerHandle, String> {
    let (action_sender, action_receiver) = channel();
    let (ready_sender, ready_receiver) = channel();
    // Never sent on: the thread drops this sender as its last act, which is what
    // `shutdown` waits for.
    let (finished_sender, finished) = channel::<()>();
    let latest = Arc::new(Mutex::new(ViewModel::default()));
    let published = Arc::clone(&latest);

    thread::Builder::new()
        .name("youta-reducer".to_owned())
        .spawn(move || {
            // Durable state and the provider are opened here so that nothing
            // lacking a `Send` bound has to cross a thread boundary.
            let store = match StateStore::open(&config) {
                Ok(store) => store,
                // One process owns durable state. A second window is an ordinary
                // outcome, so it gets the plain shared wording rather than the
                // lock's technical text.
                Err(PersistenceError::FileStateAlreadyOpen) => {
                    let _ = ready_sender.send(Err(ANOTHER_INSTANCE_MESSAGE.to_owned()));
                    return;
                }
                Err(error) => {
                    let _ = ready_sender.send(Err(format!("cannot open durable state: {error}")));
                    return;
                }
            };
            // A provider failure is not fatal: local media, playlists, and
            // history remain browsable, and the reducer reports the problem.
            let provider = configured_youtube_provider(&config.providers).unwrap_or_default();
            // The engine is not started here. The factory runs on first Play, so
            // browsing costs no decoder process, exactly as in the terminal.
            let playback = configured_playback_factory(&config);
            let mut controller = AppController::new(config, store, provider, playback);

            *published.lock().unwrap_or_else(PoisonError::into_inner) = controller.view().clone();
            if ready_sender.send(Ok(())).is_err() {
                return;
            }

            run(&app, &mut controller, &action_receiver, &published);
            // Every exit from `run` lands here, so the player process is killed
            // and durable state is flushed whether the user quit, closed the
            // window, or the window itself went away.
            let _ = controller.shutdown_for_exit();
            drop(finished_sender);
        })
        .map_err(|error| format!("cannot start the Youta reducer thread: {error}"))?;

    ready_receiver
        .recv()
        .unwrap_or_else(|_| Err("the Youta reducer stopped during startup".to_owned()))?;

    Ok(ReducerHandle {
        actions: action_sender,
        latest,
        finished: Mutex::new(finished),
    })
}

/// Applies actions, pumps workers, and publishes the view when it changes.
fn run<R: Runtime>(
    app: &AppHandle<R>,
    controller: &mut AppController,
    actions: &Receiver<Message>,
    published: &Arc<Mutex<ViewModel>>,
) {
    let mut last = controller.view().clone();
    loop {
        match actions.recv_timeout(TICK) {
            Ok(Message::Action(action)) => {
                controller.dispatch(action);
                // Drain the rest of a burst before publishing, so holding a key
                // does not produce one snapshot per repeat.
                for _ in 1..MAX_ACTIONS_PER_TICK {
                    match actions.try_recv() {
                        Ok(Message::Action(action)) => controller.dispatch(action),
                        Ok(Message::Stop) => return,
                        Err(_) => break,
                    }
                }
            }
            Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        controller.tick();

        // `ViewModel` derives `PartialEq`, so an unchanged frame costs one
        // comparison instead of a serialization and an IPC message.
        if *controller.view() != last {
            last = controller.view().clone();
            *published.lock().unwrap_or_else(PoisonError::into_inner) = last.clone();
            if app.emit(VIEW_EVENT, &last).is_err() {
                return;
            }
        }

        if controller.view().quitting {
            // Quitting is a reducer decision, so the window is told to close
            // rather than the other way round. The exit handler then waits for
            // the shutdown this function is about to return into.
            app.exit(0);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ANOTHER_INSTANCE_MESSAGE, ReducerHandle, StateStore, start};

    use tauri::App;
    use tauri::test::{MockRuntime, mock_app};
    use youta::config::Config;
    use youta::view::UiAction;

    /// Builds a reducer over a private configuration directory.
    ///
    /// The mock app is returned so the caller's binding keeps it alive for as
    /// long as the reducer publishes through its handle.
    fn reducer(config: &Config) -> (App<MockRuntime>, Result<ReducerHandle, String>) {
        let app = mock_app();
        let handle = start(app.handle().clone(), config.clone());
        (app, handle)
    }

    /// Closing the window must release the exclusive state lock through the
    /// reducer's own shutdown, not by waiting for the process to die.
    #[test]
    fn stopping_the_reducer_releases_durable_state() {
        let temporary = tempfile::tempdir().expect("temporary configuration directory");
        let config = Config::for_dir(temporary.path());

        let (_app, handle) = reducer(&config);
        handle.expect("the reducer starts").shutdown();

        // Reopening is the observable proof: the lock is exclusive, so a second
        // open succeeds only after the first store was actually closed.
        StateStore::open(&config).expect("durable state reopens after shutdown");
    }

    /// A second window is an ordinary outcome and gets the wording the terminal
    /// front-end uses, not the lock's technical text.
    #[test]
    fn a_second_reducer_is_refused_with_the_shared_message() {
        let temporary = tempfile::tempdir().expect("temporary configuration directory");
        let config = Config::for_dir(temporary.path());

        let (_first_app, first) = reducer(&config);
        let first = first.expect("the first reducer starts");
        let (_second_app, second) = reducer(&config);

        assert_eq!(second.err().as_deref(), Some(ANOTHER_INSTANCE_MESSAGE));
        first.shutdown();
    }

    /// An action from the window has to reach the controller, which is the whole
    /// point of the channel between them.
    #[test]
    fn a_dispatched_action_reaches_the_controller() {
        let temporary = tempfile::tempdir().expect("temporary configuration directory");
        let config = Config::for_dir(temporary.path());

        let (_app, handle) = reducer(&config);
        let handle = handle.expect("the reducer starts");
        assert!(!handle.snapshot().search_editing);

        handle
            .dispatch(UiAction::BeginSearch)
            .expect("the reducer accepts an action");
        let editing = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            handle.snapshot().search_editing
        });

        assert!(
            editing,
            "the published snapshot must show the search editor"
        );
        handle.shutdown();
    }
}
