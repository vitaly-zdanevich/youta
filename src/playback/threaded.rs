//! Off-thread supervision for a synchronous playback backend.
//!
//! Process backends speak a blocking request/response protocol. The mpv
//! adapter in particular performs one IPC round-trip per property, so a single
//! [`PlaybackBackend::status`] call costs eleven of them, each able to wait for
//! the adapter's socket timeout. Running that on the reducer thread makes a
//! wedged player freeze input and redraw.
//!
//! This wrapper moves the whole backend onto its own thread. The reducer reads
//! a published snapshot instead of querying the player, and lifecycle events
//! arrive through a channel. User-initiated requests still wait for their
//! acknowledgement so command failures keep surfacing exactly where they did
//! before; only the per-tick polling becomes free.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::{
    PlaybackBackend, PlaybackError, PlaybackEvent, PlaybackInput, PlaybackStatus, PlayerCommand,
    Result,
};

/// Snapshot refresh period while media is actively playing.
const ACTIVE_REFRESH: Duration = Duration::from_millis(200);

/// Snapshot refresh period while the player is idle or paused.
///
/// Youta targets battery-powered systems, so a paused player must not keep
/// waking the worker at the interactive rate.
const IDLE_REFRESH: Duration = Duration::from_secs(1);

/// Lifecycle events forwarded before the worker stops draining in one pass.
///
/// The adapter itself retains a bounded backlog; this only prevents one busy
/// pass from starving snapshot refreshes.
const MAX_EVENTS_PER_PASS: usize = 64;

/// State published by the worker and read by the reducer without blocking.
#[derive(Default)]
struct Shared {
    /// Most recent successful snapshot.
    status: PlaybackStatus,
    /// Failure observed since the reducer last read one.
    ///
    /// [`PlaybackError`] is not [`Clone`], and the reducer reports a status
    /// failure once, so the slot is taken rather than copied.
    failure: Option<PlaybackError>,
}

/// One reducer request handed to the worker thread.
enum Job {
    /// Load and start a media item.
    Play {
        /// Requested media.
        input: Box<PlaybackInput>,
        /// Acknowledgement channel.
        reply: Sender<Result<()>>,
    },
    /// Apply a playback command.
    Command {
        /// Requested command.
        command: PlayerCommand,
        /// Acknowledgement channel.
        reply: Sender<Result<()>>,
    },
    /// Stop the backend and end the worker.
    Shutdown {
        /// Acknowledgement channel.
        reply: Sender<Result<()>>,
    },
}

/// A playback backend supervised on its own thread.
pub struct ThreadedBackend {
    /// Process identity captured before the backend moves to its worker.
    process_id: Option<u32>,
    jobs: Sender<Job>,
    events: Receiver<PlaybackEvent>,
    shared: Arc<Mutex<Shared>>,
    worker: Option<JoinHandle<()>>,
}

impl ThreadedBackend {
    /// Moves `backend` onto a dedicated thread and returns its handle.
    #[must_use]
    pub fn new<B>(backend: B) -> Self
    where
        B: PlaybackBackend + Send + 'static,
    {
        let process_id = backend.process_id();
        let (job_sender, job_receiver) = channel();
        let (event_sender, event_receiver) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("youta-playback".to_owned())
            .spawn(move || run(backend, &job_receiver, &event_sender, &worker_shared))
            .ok();
        Self {
            process_id,
            jobs: job_sender,
            events: event_receiver,
            shared,
            worker,
        }
    }

    /// Sends one job and waits for the backend's own answer.
    ///
    /// Requests originate from an explicit user action, so the reducer keeps
    /// the backend's error instead of discovering it later through a snapshot.
    fn request(&self, build: impl FnOnce(Sender<Result<()>>) -> Job) -> Result<()> {
        let (reply_sender, reply_receiver) = channel();
        if self.jobs.send(build(reply_sender)).is_err() {
            return Err(worker_gone());
        }
        reply_receiver.recv().unwrap_or_else(|_| Err(worker_gone()))
    }

    /// Reads the published state, taking any pending failure.
    fn take_shared(&self) -> Result<PlaybackStatus> {
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        match shared.failure.take() {
            Some(error) => Err(error),
            None => Ok(shared.status.clone()),
        }
    }
}

/// Reports that the supervising thread is no longer available.
fn worker_gone() -> PlaybackError {
    PlaybackError::ProcessExited(": the playback supervisor stopped".to_owned())
}

/// Stores a snapshot without discarding an unread failure.
fn publish_status(shared: &Arc<Mutex<Shared>>, status: PlaybackStatus) {
    let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
    guard.status = status;
}

/// Stores a failure, keeping the first one the reducer has not read yet.
fn publish_failure(shared: &Arc<Mutex<Shared>>, error: PlaybackError) {
    let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.failure.is_none() {
        guard.failure = Some(error);
    }
}

/// Supervises one backend until the handle is dropped or shutdown is requested.
fn run<B>(
    mut backend: B,
    jobs: &Receiver<Job>,
    events: &Sender<PlaybackEvent>,
    shared: &Arc<Mutex<Shared>>,
) where
    B: PlaybackBackend,
{
    let mut refresh = ACTIVE_REFRESH;
    loop {
        match jobs.recv_timeout(refresh) {
            Ok(Job::Play { input, reply }) => {
                let _ = reply.send(backend.play(&input));
            }
            Ok(Job::Command { command, reply }) => {
                let _ = reply.send(backend.command(command));
            }
            Ok(Job::Shutdown { reply }) => {
                let _ = reply.send(backend.shutdown());
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The handle was dropped without an explicit shutdown.
            Err(RecvTimeoutError::Disconnected) => {
                let _ = backend.shutdown();
                return;
            }
        }

        for _ in 0..MAX_EVENTS_PER_PASS {
            match backend.poll_event() {
                Ok(Some(event)) => {
                    if events.send(event).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    publish_failure(shared, error);
                    break;
                }
            }
        }

        match backend.status() {
            Ok(status) => {
                refresh = if status.idle || status.paused {
                    IDLE_REFRESH
                } else {
                    ACTIVE_REFRESH
                };
                publish_status(shared, status);
            }
            Err(error) => publish_failure(shared, error),
        }
    }
}

impl PlaybackBackend for ThreadedBackend {
    fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    fn play(&mut self, input: &PlaybackInput) -> Result<()> {
        let input = Box::new(input.clone());
        self.request(|reply| Job::Play { input, reply })
    }

    fn command(&mut self, command: PlayerCommand) -> Result<()> {
        self.request(|reply| Job::Command { command, reply })
    }

    fn status(&mut self) -> Result<PlaybackStatus> {
        self.take_shared()
    }

    fn poll_event(&mut self) -> Result<Option<PlaybackEvent>> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            // A stopped worker has already published its terminal event and
            // any failure, so draining reports exhaustion rather than a second
            // error for the same cause.
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(None),
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let outcome = self.request(|reply| Job::Shutdown { reply });
        let _ = worker.join();
        outcome
    }
}

impl Drop for ThreadedBackend {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;
    use crate::playback::{PlaybackEnd, PlaybackEndReason};

    /// Backend whose calls are observable and individually controllable.
    struct FakeBackend {
        status_calls: Arc<AtomicUsize>,
        events: Arc<Mutex<Vec<PlaybackEvent>>>,
        commands: Arc<Mutex<Vec<PlayerCommand>>>,
        shutdowns: Arc<AtomicUsize>,
        status_error: Arc<Mutex<Option<PlaybackError>>>,
        command_error: Arc<Mutex<Option<PlaybackError>>>,
        paused: bool,
    }

    impl PlaybackBackend for FakeBackend {
        fn process_id(&self) -> Option<u32> {
            Some(4242)
        }

        fn play(&mut self, _input: &PlaybackInput) -> Result<()> {
            Ok(())
        }

        fn command(&mut self, command: PlayerCommand) -> Result<()> {
            self.commands
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(command);
            match self
                .command_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn status(&mut self) -> Result<PlaybackStatus> {
            self.status_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(error) = self
                .status_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return Err(error);
            }
            Ok(PlaybackStatus {
                idle: false,
                paused: self.paused,
                position: Duration::from_secs(7),
                ..PlaybackStatus::default()
            })
        }

        fn poll_event(&mut self) -> Result<Option<PlaybackEvent>> {
            Ok(self
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop())
        }

        fn shutdown(&mut self) -> Result<()> {
            self.shutdowns.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    struct Probe {
        status_calls: Arc<AtomicUsize>,
        events: Arc<Mutex<Vec<PlaybackEvent>>>,
        commands: Arc<Mutex<Vec<PlayerCommand>>>,
        shutdowns: Arc<AtomicUsize>,
        status_error: Arc<Mutex<Option<PlaybackError>>>,
        command_error: Arc<Mutex<Option<PlaybackError>>>,
    }

    fn threaded(paused: bool) -> (ThreadedBackend, Probe) {
        let probe = Probe {
            status_calls: Arc::new(AtomicUsize::new(0)),
            events: Arc::new(Mutex::new(Vec::new())),
            commands: Arc::new(Mutex::new(Vec::new())),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            status_error: Arc::new(Mutex::new(None)),
            command_error: Arc::new(Mutex::new(None)),
        };
        let backend = FakeBackend {
            status_calls: Arc::clone(&probe.status_calls),
            events: Arc::clone(&probe.events),
            commands: Arc::clone(&probe.commands),
            shutdowns: Arc::clone(&probe.shutdowns),
            status_error: Arc::clone(&probe.status_error),
            command_error: Arc::clone(&probe.command_error),
            paused,
        };
        (ThreadedBackend::new(backend), probe)
    }

    /// Waits for a condition without pinning the test to one exact timing.
    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn process_identity_survives_moving_the_backend_to_its_worker() {
        let (mut handle, _) = threaded(false);
        assert_eq!(handle.process_id(), Some(4242));
        handle.shutdown().expect("stop worker");
    }

    #[test]
    fn status_reads_a_published_snapshot_without_calling_the_backend() {
        let (mut handle, probe) = threaded(false);
        assert!(wait_until(|| probe.status_calls.load(Ordering::Relaxed) > 0));

        let before = probe.status_calls.load(Ordering::Relaxed);
        for _ in 0..50 {
            let status = handle.status().expect("published snapshot");
            assert_eq!(status.position, Duration::from_secs(7));
        }
        let after = probe.status_calls.load(Ordering::Relaxed);

        assert!(
            after - before < 50,
            "reducer reads must not become backend round-trips: {before} -> {after}"
        );
    }

    #[test]
    fn a_status_failure_reaches_the_reducer_exactly_once() {
        let (mut handle, probe) = threaded(false);
        assert!(wait_until(|| probe.status_calls.load(Ordering::Relaxed) > 0));

        *probe
            .status_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Some(PlaybackError::Protocol("wedged".to_owned()));

        assert!(wait_until(|| handle.status().is_err()));
        assert!(
            wait_until(|| handle.status().is_ok()),
            "the failure slot must not latch after the reducer read it"
        );
    }

    #[test]
    fn commands_keep_returning_the_backend_error_synchronously() {
        let (mut handle, probe) = threaded(false);
        *probe
            .command_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Some(PlaybackError::DirectProfileRestriction("software volume"));

        let error = handle
            .command(PlayerCommand::SetVolume(30))
            .expect_err("the backend error must reach the caller");
        assert!(matches!(
            error,
            PlaybackError::DirectProfileRestriction("software volume")
        ));
        assert_eq!(
            probe
                .commands
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_slice(),
            [PlayerCommand::SetVolume(30)]
        );
    }

    #[test]
    fn lifecycle_events_are_forwarded_in_protocol_order() {
        let (mut handle, probe) = threaded(false);
        probe
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend([
                PlaybackEvent::Ended(PlaybackEnd {
                    reason: PlaybackEndReason::Eof,
                    error: None,
                    file_error: None,
                    diagnostic: None,
                }),
                PlaybackEvent::PlaybackStarted,
                PlaybackEvent::MediaLoaded,
            ]);

        let mut seen = Vec::new();
        assert!(wait_until(|| {
            while let Ok(Some(event)) = handle.poll_event() {
                seen.push(event);
            }
            seen.len() == 3
        }));
        assert_eq!(seen[0], PlaybackEvent::MediaLoaded);
        assert_eq!(seen[1], PlaybackEvent::PlaybackStarted);
        assert!(matches!(seen[2], PlaybackEvent::Ended(_)));
    }

    #[test]
    fn a_paused_player_refreshes_less_often_than_an_active_one() {
        let (_active, active_probe) = threaded(false);
        let (_paused, paused_probe) = threaded(true);
        assert!(wait_until(|| {
            active_probe.status_calls.load(Ordering::Relaxed) > 0
                && paused_probe.status_calls.load(Ordering::Relaxed) > 0
        }));

        thread::sleep(Duration::from_millis(700));
        let active = active_probe.status_calls.load(Ordering::Relaxed);
        let paused = paused_probe.status_calls.load(Ordering::Relaxed);
        assert!(
            active > paused,
            "an idle player must not poll at the interactive rate: {active} vs {paused}"
        );
    }

    #[test]
    fn shutdown_stops_the_backend_once_and_survives_the_later_drop() {
        let (mut handle, probe) = threaded(false);
        handle.shutdown().expect("clean shutdown");
        assert_eq!(probe.shutdowns.load(Ordering::Relaxed), 1);

        handle.shutdown().expect("a second shutdown is a no-op");
        drop(handle);
        assert_eq!(probe.shutdowns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dropping_the_handle_stops_the_backend() {
        let (handle, probe) = threaded(false);
        drop(handle);
        assert!(wait_until(|| probe.shutdowns.load(Ordering::Relaxed) == 1));
    }
}
