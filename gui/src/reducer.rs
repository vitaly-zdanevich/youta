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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use url::Url;

// The runtime is a type parameter so the reducer can be driven by Tauri's mock
// runtime in tests. Nothing here depends on the real windowing backend.
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_clipboard_manager::ClipboardExt;

use serde::Serialize;

use youta::app::AppController;
use youta::config::Config;
use youta::keymap::{KeyPress, PopupGeometry, key_action};
use youta::persistence::{ANOTHER_INSTANCE_MESSAGE, PersistenceError, StateStore};
use youta::playback::configured_playback_factory;
use youta::providers::configured_youtube_provider;
use youta::text_file_open::{TextFileOpenLifecycle, spawn_detached_text_file_open};
use youta::view::{UiAction, UiController, ViewModel, WaveformView};
use youta::waveform::Peak;

use crate::desktop::{Announced, WindowFocus};
use crate::media_keys::MediaCommand;

/// Event name carrying a changed snapshot to the window.
pub const VIEW_EVENT: &str = "youta://view";

/// Event name carrying only the fields a playing item changes.
pub const PLAYBACK_EVENT: &str = "youta://playback";

/// The fields playback rewrites several times a second.
///
/// Measured: a fifty-row snapshot is about 22 KiB, of which the rows are 20,
/// and playback republishes four times a second. Sending the whole snapshot for
/// a moved position costs roughly 86 KiB/s to retransmit a list that did not
/// change. This carries the part that did.
///
/// Which fields belong here is not asserted by hand. The reducer copies this
/// group onto the previous snapshot and compares: if the result equals the new
/// snapshot, nothing outside the group moved. A field missing from the group
/// therefore causes a full snapshot, never a stale window.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct PlaybackTick {
    playback: youta::playback::PlaybackStatus,
    radio_now_playing: Option<String>,
    playback_starting: bool,
    playback_start_animation_frame: usize,
    search_animation_frame: usize,
    local_fingerprint_animation_frame: usize,
}

impl PlaybackTick {
    /// Copies the high-frequency group out of a snapshot.
    fn of(view: &ViewModel) -> Self {
        Self {
            playback: view.playback.clone(),
            radio_now_playing: view.radio_now_playing.clone(),
            playback_starting: view.playback_starting,
            playback_start_animation_frame: view.playback_start_animation_frame,
            search_animation_frame: view.search_animation_frame,
            local_fingerprint_animation_frame: view.local_fingerprint_animation_frame,
        }
    }

    /// Writes this group onto a snapshot.
    fn apply_to(&self, view: &mut ViewModel) {
        view.playback = self.playback.clone();
        view.radio_now_playing = self.radio_now_playing.clone();
        view.playback_starting = self.playback_starting;
        view.playback_start_animation_frame = self.playback_start_animation_frame;
        view.search_animation_frame = self.search_animation_frame;
        view.local_fingerprint_animation_frame = self.local_fingerprint_animation_frame;
    }
}

/// Largest number of waveform columns the window may ask for at once.
///
/// A window is a few thousand device pixels wide at most. The cap is here so a
/// malformed or hostile column count cannot make the reducer reduce and
/// allocate without bound on a request that arrives from a web view.
const MAX_WAVEFORM_COLUMNS: usize = 8192;

/// Encodes one peak per column as a little-endian minimum/maximum pair.
///
/// Peaks travel as bytes rather than as JSON because they are the one part of
/// the view that is genuinely large: a window-wide envelope is thousands of
/// numbers, which JSON inflates roughly tenfold for values that are already
/// exactly sixteen bits. The endianness is fixed here rather than left to the
/// platform so the reader can be explicit about it too.
fn encode_peaks(peaks: &[Peak]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(peaks.len().saturating_mul(4));
    for peak in peaks {
        bytes.extend_from_slice(&peak.minimum.to_le_bytes());
        bytes.extend_from_slice(&peak.maximum.to_le_bytes());
    }
    bytes
}

/// Reduces a snapshot's waveform for one exact owner, or returns nothing.
///
/// The caller names the generation it drew, and gets bytes only if that
/// generation still owns the waveform. This is the same ownership rule
/// `UiAction::ActivateWaveformTimecode` enforces on the way back in, and it
/// matters for the same reason: a request still in flight while the selection
/// moves would otherwise paint one file's envelope over another's, and a click
/// on those pixels would seek the wrong media.
fn peaks_for(view: &ViewModel, generation: u64, columns: usize) -> Vec<u8> {
    let columns = columns.min(MAX_WAVEFORM_COLUMNS);
    if columns == 0 {
        return Vec::new();
    }
    let WaveformView::Ready {
        generation: owner,
        pyramid,
        ..
    } = &view.waveform
    else {
        return Vec::new();
    };
    if *owner != generation {
        return Vec::new();
    }
    encode_peaks(&pyramid.reduced_for_width(columns))
}

/// Local artwork URLs remembered from recently published snapshots.
///
/// One selection publishes at most a handful — the panel's cover and the rows
/// around it — so this holds several screens' worth of movement.
const MAX_PUBLISHED_LOCAL_ARTWORK: usize = 64;

/// The local artwork a published snapshot named, newest last.
///
/// A local cover is a real file: an opaque entry in Youta's private artwork
/// cache, or the image a downloader left beside the media. The window renders it
/// with an ordinary `<img src>`, so the artwork endpoint has to be able to read
/// those files — and there is no path pattern that separates the user's own
/// `cover.jpg` from the rest of their filesystem.
///
/// This is that separation, and it is an allowlist rather than a rule: the
/// endpoint serves a file only when the reducer itself published its URL in a
/// snapshot the window was given. A `file:` URL that arrives from a provider is
/// therefore refused, because no snapshot ever named it.
///
/// It remembers several selections rather than only the current one, since an
/// image request can arrive after the selection that produced it has moved on.
#[derive(Default)]
pub struct PublishedArtwork {
    recent: Mutex<VecDeque<Url>>,
}

impl PublishedArtwork {
    /// Records every local artwork URL one snapshot names.
    fn record(&self, view: &ViewModel) {
        let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        for url in local_artwork_urls(view) {
            if recent.contains(url) {
                continue;
            }
            recent.push_back(url.clone());
            while recent.len() > MAX_PUBLISHED_LOCAL_ARTWORK {
                recent.pop_front();
            }
        }
    }

    /// Whether a published snapshot named this exact artwork.
    fn published(&self, source: &Url) -> bool {
        self.recent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(source)
    }
}

/// Yields the local artwork one snapshot names, wherever it appears.
///
/// Remote artwork is left out because it needs no permission: the endpoint
/// fetches an `http`/`https` source through Youta's guarded agent, which applies
/// its own rules to any URL at all.
fn local_artwork_urls(view: &ViewModel) -> impl Iterator<Item = &Url> {
    let details = view.details.iter().flat_map(|details| {
        details
            .thumbnail_url
            .iter()
            .chain(details.expanded_thumbnail_url.iter())
    });
    view.rows
        .iter()
        .chain(&view.subscriptions.sources)
        .chain(&view.subscriptions.items)
        .filter_map(|row| row.thumbnail_url.as_ref())
        .chain(details)
        .filter(|url| url.scheme() == "file")
}

/// Reducer wake-up period.
///
/// The reducer also wakes immediately for every dispatched action, so this only
/// bounds how long a worker response waits before reaching the window.
const TICK: Duration = Duration::from_millis(100);

/// Actions applied before the next snapshot is published.
const MAX_ACTIONS_PER_TICK: usize = 64;

/// How often the optional traffic meter reports.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

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
    /// A key press, still to be resolved against the live view.
    ///
    /// The window deliberately does not resolve this itself. Which action a key
    /// means depends on the modal state, and that state lives here, so a key
    /// mapped in the window would be mapped against a snapshot that may already
    /// be stale.
    Key {
        /// The press, in the shared vocabulary.
        press: KeyPress,
        /// Main-list rows the window is showing, for page-sized movement.
        page_rows: Option<usize>,
        /// Rendered scroll state of the scrollable popups.
        popups: Option<PopupGeometry>,
    },
    /// A press on the desktop's own media surface.
    ///
    /// Carried rather than resolved for the same reason a key press is: Play
    /// and Pause name a destination, so answering them anywhere but here would
    /// answer them against a snapshot that may already have moved past.
    Media(souvlaki::MediaControlEvent),
    /// Stop the engine, flush durable state, and release the state lock.
    Stop,
}

/// Permission for the app exit initiated by the reducer itself.
///
/// Native quit events arrive on Tauri's event loop, while confirmation and
/// other stateful actions are serialized on the reducer thread. An unapproved
/// native exit therefore has to become an ordinary reducer `Quit`; consulting a
/// published snapshot would race an action that is already queued but not yet
/// visible. Once that `Quit` is accepted, this token lets the resulting
/// [`AppHandle::exit`] request pass through the same native callback. The token
/// remains set because the reducer returns immediately after authorization; if
/// a platform emits more than one exit event, none should revive a controller
/// that has already committed to shutdown.
#[derive(Clone, Debug, Default)]
struct ExitAuthorization(Arc<AtomicBool>);

impl ExitAuthorization {
    fn authorize(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn authorized(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Handle used by IPC commands to reach the reducer thread.
pub struct ReducerHandle {
    actions: Sender<Message>,
    latest: Arc<Mutex<ViewModel>>,
    artwork: Arc<PublishedArtwork>,
    /// A `Receiver` is `Send` but not `Sync`, and Tauri shares managed state
    /// across threads, so the exit path takes this lock to wait on it.
    finished: Mutex<Receiver<()>>,
    /// Permission set only for the native exit requested by the reducer.
    exit_authorization: ExitAuthorization,
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

    /// Routes a native exit through the reducer unless the reducer authorized it.
    ///
    /// The return value says whether the caller must prevent the current native
    /// exit. A `true` result means `Quit` was queued behind every action already
    /// sent by the window. If the reducer has stopped and cannot receive that
    /// action, the native exit is allowed so a broken background thread cannot
    /// leave an uncloseable process.
    #[must_use]
    pub fn route_exit_request(&self) -> bool {
        if self.exit_authorization.authorized() {
            return false;
        }
        self.dispatch(UiAction::Quit).is_ok()
    }

    /// Queues one key press for the reducer to resolve and apply.
    ///
    /// # Errors
    ///
    /// Returns a message when the reducer thread is no longer running.
    pub fn key(
        &self,
        press: KeyPress,
        page_rows: Option<usize>,
        popups: Option<PopupGeometry>,
    ) -> Result<(), String> {
        self.actions
            .send(Message::Key {
                press,
                page_rows,
                popups,
            })
            .map_err(|_| "the Youta reducer stopped".to_owned())
    }

    /// Queues one media-surface event for the reducer to resolve and apply.
    ///
    /// # Errors
    ///
    /// Returns a message when the reducer thread is no longer running.
    pub fn media(&self, event: souvlaki::MediaControlEvent) -> Result<(), String> {
        self.actions
            .send(Message::Media(event))
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

    /// Whether this local artwork URL appeared in a snapshot the window was
    /// given.
    ///
    /// See [`PublishedArtwork`] for why the artwork endpoint asks.
    #[must_use]
    pub fn published_local_artwork(&self, source: &Url) -> bool {
        self.artwork.published(source)
    }

    /// Returns the waveform envelope reduced to exactly `columns` columns.
    ///
    /// An empty result means "not yours, or nothing to draw" — the window
    /// re-asks when the next snapshot names a generation. See [`peaks_for`] for
    /// why the generation is part of the question.
    #[must_use]
    pub fn waveform_peaks(&self, generation: u64, columns: usize) -> Vec<u8> {
        peaks_for(
            &self.latest.lock().unwrap_or_else(PoisonError::into_inner),
            generation,
            columns,
        )
    }

    /// Stops playback and durable state before the process ends.
    ///
    /// External exits can bypass the window's reducer-owned close request, so
    /// the event-loop fallback still needs a synchronous shutdown path.
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
pub fn start<R: Runtime>(
    app: AppHandle<R>,
    config: Config,
    focus: WindowFocus,
) -> Result<ReducerHandle, String> {
    let (action_sender, action_receiver) = channel();
    let (ready_sender, ready_receiver) = channel();
    // Never sent on: the thread drops this sender as its last act, which is what
    // `shutdown` waits for.
    let (finished_sender, finished) = channel::<()>();
    let latest = Arc::new(Mutex::new(ViewModel::default()));
    let published = Arc::clone(&latest);
    let artwork = Arc::new(PublishedArtwork::default());
    let published_artwork = Arc::clone(&artwork);
    let exit_authorization = ExitAuthorization::default();
    let reducer_exit_authorization = exit_authorization.clone();

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

            published_artwork.record(controller.view());
            *published.lock().unwrap_or_else(PoisonError::into_inner) = controller.view().clone();
            if ready_sender.send(Ok(())).is_err() {
                return;
            }

            run(
                &app,
                &mut controller,
                &action_receiver,
                &published,
                &published_artwork,
                &focus,
                &reducer_exit_authorization,
            );
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
        artwork,
        finished: Mutex::new(finished),
        exit_authorization,
    })
}

/// Applies one message, returning whether the reducer should keep running.
///
/// A key is resolved here rather than in the window, against the view the
/// controller holds right now, which is what makes the ordered modal
/// precedence in [`youta::keymap`] mean the same thing in both front-ends. A
/// media key is resolved here for the narrower version of the same reason: it
/// arrives from outside the process entirely, and Play means "resume" rather
/// than "toggle".
fn apply<R: Runtime>(app: &AppHandle<R>, controller: &mut AppController, message: Message) -> bool {
    match message {
        Message::Action(action) => controller.dispatch(action),
        Message::Key {
            press,
            page_rows,
            popups,
        } => {
            // An unbound key is not an error: most keys mean nothing in most
            // modal states, exactly as in the terminal.
            if let Some(action) = key_action(press, controller.view(), page_rows, popups) {
                controller.dispatch(action);
            }
        }
        Message::Media(event) => match crate::media_keys::command_for(&event, controller.view()) {
            Some(MediaCommand::Act(action)) => controller.dispatch(action),
            // Showing a hidden window is a window-manager operation, which is
            // why the reducer has no word for it. See `desktop::show_window`.
            Some(MediaCommand::Show) => crate::desktop::show_window(app),
            None => {}
        },
        Message::Stop => return false,
    }
    true
}

/// Counts what the whole-snapshot protocol actually costs.
///
/// The plan calls for a sectioned diff with per-section generations, but that
/// is only worth its complexity if the whole-snapshot protocol is too
/// expensive, and nobody had measured it. Setting `YOUTA_GUI_IPC_STATS=1`
/// prints a line per interval so the decision rests on numbers.
struct TrafficMeter {
    enabled: bool,
    snapshots: u64,
    ticks: u64,
    bytes: u64,
    largest: usize,
    since: Instant,
}

impl TrafficMeter {
    /// Reads the opt-in once; measurement is off unless asked for.
    fn new() -> Self {
        Self {
            enabled: std::env::var("YOUTA_GUI_IPC_STATS").as_deref() == Ok("1"),
            snapshots: 0,
            ticks: 0,
            bytes: 0,
            largest: 0,
            since: Instant::now(),
        }
    }

    /// Records one published playback tick.
    fn record_tick(&mut self, tick: &PlaybackTick) {
        if self.enabled {
            self.ticks += 1;
            self.bytes += serde_json::to_vec(tick).map_or(0, |bytes| bytes.len()) as u64;
        }
    }

    /// Records one published snapshot and reports on each interval boundary.
    fn record(&mut self, view: &ViewModel) {
        if !self.enabled {
            return;
        }
        let encoded = serde_json::to_vec(view).map_or(0, |bytes| bytes.len());
        self.snapshots += 1;
        self.bytes += encoded as u64;
        self.largest = self.largest.max(encoded);

        let elapsed = self.since.elapsed();
        if elapsed < STATS_INTERVAL {
            return;
        }
        let seconds = elapsed.as_secs_f64();
        eprintln!(
            "youta ipc: {:.1} snapshots/s, {:.1} ticks/s, {:.1} KiB/s, largest snapshot {} B",
            self.snapshots as f64 / seconds,
            self.ticks as f64 / seconds,
            self.bytes as f64 / seconds / 1024.0,
            self.largest,
        );
        self.snapshots = 0;
        self.ticks = 0;
        self.bytes = 0;
        self.largest = 0;
        self.since = Instant::now();
    }
}

/// Performs the side effects the controller planned but cannot carry out.
///
/// Both of these are deliberately not the reducer's to do. The controller
/// decides *that* a link should be copied and *which* command opens a text
/// file; how a clipboard is reached and how a child process is started differ
/// between a terminal and a window, so each front-end supplies its own half.
///
/// The terminal's half is in `youta::tui`: a native helper or an OSC 52 escape
/// written to its own tty. Neither exists here — a window has the platform
/// clipboard directly, and an escape sequence would be written into a stdout
/// nobody reads and then reported as a successful copy.
fn serve_side_effects<R: Runtime>(app: &AppHandle<R>, controller: &mut AppController) {
    if let Some(request) = controller.take_clipboard_request() {
        controller.report_clipboard_result(
            app.clipboard()
                .write_text(request.text)
                .map(|()| "the system clipboard".to_owned())
                .map_err(|error| error.to_string()),
        );
    }
    // `local-browser` is always on in this binary — `sources` selects it — so
    // the text-file half of `UiController` is always present here.
    if let Some(plan) = controller.take_text_file_open_plan() {
        // A window is never a Linux virtual console, so the controller always
        // plans the detached system opener here. `spawn_detached_text_file_open`
        // refuses anything else rather than launching a terminal editor into a
        // process that has no terminal.
        controller.report_text_file_open_result(
            spawn_detached_text_file_open(&plan).map(|()| TextFileOpenLifecycle::Detached),
        );
    }
}

/// Applies actions, pumps workers, and publishes the view when it changes.
fn run<R: Runtime>(
    app: &AppHandle<R>,
    controller: &mut AppController,
    actions: &Receiver<Message>,
    published: &Arc<Mutex<ViewModel>>,
    artwork: &Arc<PublishedArtwork>,
    focus: &WindowFocus,
    exit_authorization: &ExitAuthorization,
) {
    let mut last = controller.view().clone();
    let mut traffic = TrafficMeter::new();
    let mut announced = Announced::default();
    // Registered here rather than in `setup`, because reading a native window
    // handle asks the main thread for it and the main thread is inside `setup`
    // waiting for this thread to report that durable state opened. The surface
    // is dropped with this function, which is what detaches it from the desktop.
    let mut media = crate::media_keys::Surface::install(app);
    // Said once before the loop, because the loop only speaks when the snapshot
    // *changes* and the surface has to be right before anything happens: a
    // player that has registered but stated nothing shows a desktop panel full
    // of whatever the platform defaults to.
    if let Some(media) = media.as_mut() {
        media.publish(&last);
    }
    loop {
        match actions.recv_timeout(TICK) {
            Ok(message @ (Message::Action(_) | Message::Key { .. } | Message::Media(_))) => {
                if !apply(app, controller, message) {
                    return;
                }
                // Drain the rest of a burst before publishing, so holding a key
                // does not produce one snapshot per repeat.
                for _ in 1..MAX_ACTIONS_PER_TICK {
                    match actions.try_recv() {
                        Ok(message) => {
                            if !apply(app, controller, message) {
                                return;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        serve_side_effects(app, controller);
        controller.tick();

        // `ViewModel` derives `PartialEq`, so an unchanged frame costs one
        // comparison instead of a serialization and an IPC message.
        if *controller.view() != last {
            let current = controller.view();
            let tick = PlaybackTick::of(current);

            // Ask whether the high-frequency group explains the whole change.
            // Getting this wrong can only cost a full snapshot, never a stale
            // window, because the comparison is against the real thing.
            let mut probe = last.clone();
            tick.apply_to(&mut probe);
            let only_playback_moved = probe == *current;

            last = current.clone();
            // Recorded before the snapshot leaves, so the artwork endpoint can
            // never be asked about a URL it has not seen yet.
            artwork.record(&last);
            *published.lock().unwrap_or_else(PoisonError::into_inner) = last.clone();

            // The window title, the tray, and a track-change notification read
            // the same published snapshot the page does, so they cannot show
            // something the page never showed.
            if let Some(announcement) =
                crate::desktop::announce(&last, focus.focused(), &mut announced)
            {
                crate::desktop::perform(app, &announcement);
            }
            // The desktop's own media panel reads the same snapshot, so what it
            // shows can never be something the window never showed either.
            if let Some(media) = media.as_mut() {
                media.publish(&last);
            }

            let emitted = if only_playback_moved {
                traffic.record_tick(&tick);
                app.emit(PLAYBACK_EVENT, &tick)
            } else {
                traffic.record(&last);
                app.emit(VIEW_EVENT, &last)
            };
            if emitted.is_err() {
                return;
            }
        }

        if controller.view().quitting {
            // Quitting is a reducer decision, so the window is told to close
            // rather than the other way round. The exit handler then waits for
            // the shutdown this function is about to return into.
            exit_authorization.authorize();
            app.exit(0);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ANOTHER_INSTANCE_MESSAGE, ExitAuthorization, Message, PublishedArtwork, ReducerHandle,
        StateStore, WindowFocus, start,
    };

    use super::{PlaybackTick, ViewModel};

    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};

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
        let handle = start(app.handle().clone(), config.clone(), WindowFocus::default());
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
        //
        // `shutdown` deliberately stops waiting after its grace period rather
        // than hang a closing window on a wedged reducer, so on a slow enough
        // machine it returns while the reducer thread is still dropping the
        // store. The release is therefore *eventual* from this thread's point
        // of view, and the proof has to poll for it rather than demand it in
        // the first attempt.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match StateStore::open(&config) {
                Ok(reopened) => {
                    drop(reopened);
                    break;
                }
                Err(error) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "durable state never reopened after shutdown: {error:?}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
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

    /// A native quit can arrive before the reducer publishes the confirmation
    /// action immediately preceding it. Both must therefore travel through the
    /// same FIFO rather than deciding from the older published snapshot.
    #[test]
    fn an_external_exit_queues_quit_behind_an_unpublished_confirmation() {
        let (actions, inbox) = channel();
        let (_finished_sender, finished) = channel();
        let handle = ReducerHandle {
            actions,
            latest: Arc::new(Mutex::new(ViewModel::default())),
            artwork: Arc::new(PublishedArtwork::default()),
            finished: Mutex::new(finished),
            exit_authorization: ExitAuthorization::default(),
        };

        handle
            .dispatch(UiAction::ConfirmGitHubIssueSubmission)
            .expect("queue confirmation");
        assert!(
            handle.route_exit_request(),
            "an external request must be prevented while reducer Quit is queued"
        );
        assert!(matches!(
            inbox.recv().expect("confirmation message"),
            Message::Action(UiAction::ConfirmGitHubIssueSubmission)
        ));
        assert!(matches!(
            inbox.recv().expect("quit message"),
            Message::Action(UiAction::Quit)
        ));

        handle.exit_authorization.authorize();
        assert!(
            !handle.route_exit_request(),
            "the reducer's own exit must be allowed"
        );
        assert!(
            !handle.route_exit_request(),
            "repeated native events must not revive a committed shutdown"
        );
        assert!(
            inbox.try_recv().is_err(),
            "an authorized exit must not queue another Quit"
        );
    }

    /// A position tick must travel as a playback update, not as a whole
    /// snapshot: that is the entire point of the split, and a change anywhere
    /// else must fall back to the full snapshot.
    #[test]
    fn the_playback_group_explains_a_position_tick_and_nothing_more() {
        use std::time::Duration;

        let mut before = ViewModel::default();
        before.rows = vec![youta::view::RowView {
            title: "a listed item".to_owned(),
            ..youta::view::RowView::default()
        }];
        let mut after = before.clone();
        after.playback.position = Duration::from_secs(41);

        let tick = PlaybackTick::of(&after);
        let mut probe = before.clone();
        tick.apply_to(&mut probe);
        assert_eq!(probe, after, "a moved position must be fully explained");

        // A changed selection is outside the group, so the probe must disagree
        // and the reducer must fall back to publishing everything.
        let mut also_selected = after.clone();
        also_selected.selected = 1;
        let mut probe = before;
        PlaybackTick::of(&also_selected).apply_to(&mut probe);
        assert_ne!(probe, also_selected, "a list change must force a snapshot");
    }

    /// The split only pays if the tick is far smaller than the snapshot.
    #[test]
    fn a_playback_tick_is_a_fraction_of_a_snapshot() {
        let mut view = ViewModel::default();
        view.rows = (0..50)
            .map(|index| youta::view::RowView {
                title: format!("A reasonably long provider video title, number {index}"),
                subtitle: "Some Channel Name · 3 weeks ago · 1.2M views".to_owned(),
                ..youta::view::RowView::default()
            })
            .collect();

        let snapshot = serde_json::to_vec(&view).expect("encode snapshot").len();
        let tick = serde_json::to_vec(&PlaybackTick::of(&view))
            .expect("encode tick")
            .len();
        println!("snapshot {snapshot} B, playback tick {tick} B");
        assert!(
            tick * 20 < snapshot,
            "a tick of {tick} B against a {snapshot} B snapshot does not justify \
             the second channel"
        );
    }

    /// Peaks must arrive as fixed-width little-endian pairs, because the window
    /// reads them with an explicit `DataView` and nothing else describes the
    /// layout.
    #[test]
    fn peaks_encode_as_little_endian_minimum_maximum_pairs() {
        use super::{Peak, encode_peaks};

        let encoded = encode_peaks(&[
            Peak {
                minimum: -2,
                maximum: 258,
            },
            Peak {
                minimum: i16::MIN,
                maximum: i16::MAX,
            },
        ]);
        assert_eq!(
            encoded,
            vec![0xFE, 0xFF, 0x02, 0x01, 0x00, 0x80, 0xFF, 0x7F],
            "one column is four bytes: minimum then maximum, low byte first"
        );
        assert!(encode_peaks(&[]).is_empty());
    }

    /// A window that asks for a generation the reducer no longer owns must get
    /// nothing.
    ///
    /// Serving the current waveform to a request naming an older one is exactly
    /// the failure the ownership rule exists to prevent: the window would paint
    /// one file's envelope and then seek another file with the pixel it clicked.
    #[test]
    fn waveform_peaks_are_refused_for_a_generation_that_is_not_current() {
        use std::sync::Arc;
        use std::time::Duration;

        use youta::domain::{MediaId, SourceKind};
        use youta::view::WaveformView;
        use youta::waveform::{Peak, PeakPyramid};

        // Nothing owns the waveform yet, so even the right question has no answer.
        let mut view = ViewModel::default();
        assert!(super::peaks_for(&view, 4, 8).is_empty());

        view.waveform = WaveformView::Ready {
            media_id: MediaId::new(SourceKind::Local, "/music/owner.flac"),
            generation: 4,
            duration: Duration::from_secs(2),
            pyramid: Arc::new(PeakPyramid::from_peaks(
                vec![
                    Peak {
                        minimum: -100,
                        maximum: 100,
                    };
                    8
                ],
                1,
                8,
            )),
        };

        assert_eq!(
            super::peaks_for(&view, 4, 8).len(),
            32,
            "the owning generation gets one four-byte column per requested column"
        );
        assert!(
            super::peaks_for(&view, 3, 8).is_empty(),
            "a stale generation must get nothing rather than another file's peaks"
        );
        assert!(super::peaks_for(&view, 4, 0).is_empty());
        assert_eq!(
            super::peaks_for(&view, 4, usize::MAX).len(),
            super::MAX_WAVEFORM_COLUMNS * 4,
            "an unbounded column count must be clamped, not honoured"
        );
    }

    /// The artwork endpoint's permission is exactly "the reducer published
    /// this", so a `file:` URL that never appeared in a snapshot must not be
    /// recognized — and a remote one must not be recorded at all, because it
    /// needs no permission and would only crowd the set.
    #[test]
    fn only_local_artwork_from_a_published_snapshot_is_recognized() {
        use super::PublishedArtwork;
        use youta::view::{DetailView, RowView};

        let cover = url("file:///music/album/.cache/9f2c.image");
        let sidecar = url("file:///music/album/Track.webp");
        let remote = url("https://images.example/cover.jpg");
        let never_published = url("file:///etc/passwd");

        let published = PublishedArtwork::default();
        let mut view = ViewModel::default();
        view.details = Some(DetailView {
            thumbnail_url: Some(cover.clone()),
            ..DetailView::default()
        });
        view.rows = vec![
            RowView {
                thumbnail_url: Some(sidecar.clone()),
                ..RowView::default()
            },
            RowView {
                thumbnail_url: Some(remote.clone()),
                ..RowView::default()
            },
        ];
        published.record(&view);

        assert!(published.published(&cover));
        assert!(published.published(&sidecar));
        assert!(!published.published(&remote));
        assert!(!published.published(&never_published));
    }

    /// An image request can arrive after the selection that produced it moved
    /// on, so a covered selection stays readable for a while — but not forever.
    #[test]
    fn a_recent_selection_stays_readable_within_a_bounded_history() {
        use super::{MAX_PUBLISHED_LOCAL_ARTWORK, PublishedArtwork};
        use youta::view::DetailView;

        let published = PublishedArtwork::default();
        let first = url("file:///music/first.webp");
        for index in 0..MAX_PUBLISHED_LOCAL_ARTWORK {
            let mut view = ViewModel::default();
            view.details = Some(DetailView {
                thumbnail_url: Some(url(&format!("file:///music/{index}.webp"))),
                ..DetailView::default()
            });
            published.record(&view);
        }
        assert!(
            published.published(&url("file:///music/0.webp")),
            "a selection this recent must still be readable"
        );

        // One more selection than the history holds evicts the oldest.
        let mut view = ViewModel::default();
        view.details = Some(DetailView {
            thumbnail_url: Some(first.clone()),
            ..DetailView::default()
        });
        published.record(&view);
        assert!(published.published(&first));
        assert!(!published.published(&url("file:///music/0.webp")));
    }

    fn url(value: &str) -> super::Url {
        super::Url::parse(value).expect("parse artwork URL fixture")
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
