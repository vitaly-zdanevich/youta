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
use std::time::{Duration, Instant};

// The runtime is a type parameter so the reducer can be driven by Tauri's mock
// runtime in tests. Nothing here depends on the real windowing backend.
use tauri::{AppHandle, Emitter, Runtime};

use serde::Serialize;

use youta::app::AppController;
use youta::config::Config;
use youta::keymap::{KeyPress, PopupGeometry, key_action};
use youta::persistence::{ANOTHER_INSTANCE_MESSAGE, PersistenceError, StateStore};
use youta::playback::configured_playback_factory;
use youta::providers::configured_youtube_provider;
use youta::view::{UiAction, UiController, ViewModel, WaveformView};
use youta::waveform::Peak;

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

    /// Returns the most recently published snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ViewModel {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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

/// Applies one message, returning whether the reducer should keep running.
///
/// A key is resolved here rather than in the window, against the view the
/// controller holds right now, which is what makes the seventeen-level modal
/// precedence in [`youta::keymap`] mean the same thing in both front-ends.
fn apply(controller: &mut AppController, message: Message) -> bool {
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

/// Applies actions, pumps workers, and publishes the view when it changes.
fn run<R: Runtime>(
    app: &AppHandle<R>,
    controller: &mut AppController,
    actions: &Receiver<Message>,
    published: &Arc<Mutex<ViewModel>>,
) {
    let mut last = controller.view().clone();
    let mut traffic = TrafficMeter::new();
    loop {
        match actions.recv_timeout(TICK) {
            Ok(message @ (Message::Action(_) | Message::Key { .. })) => {
                if !apply(controller, message) {
                    return;
                }
                // Drain the rest of a burst before publishing, so holding a key
                // does not produce one snapshot per repeat.
                for _ in 1..MAX_ACTIONS_PER_TICK {
                    match actions.try_recv() {
                        Ok(message) => {
                            if !apply(controller, message) {
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
            *published.lock().unwrap_or_else(PoisonError::into_inner) = last.clone();

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
            app.exit(0);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ANOTHER_INSTANCE_MESSAGE, ReducerHandle, StateStore, start};

    use super::{PlaybackTick, ViewModel};

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
