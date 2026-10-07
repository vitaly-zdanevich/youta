//! Off-thread supervision for a synchronous playback backend.
//!
//! Process backends speak a blocking request/response protocol. The mpv
//! adapter can wait for command acknowledgements or an initial property
//! snapshot. Running that on the reducer thread makes a wedged player freeze
//! input and redraw. Notification-capable backends wake this worker directly;
//! other adapters retain their existing bounded refresh fallback.
//!
//! This wrapper moves the whole backend onto its own thread. The reducer reads
//! a published snapshot instead of querying the player, and lifecycle events
//! arrive through a channel. User-initiated requests still wait for their
//! acknowledgement so command failures keep surfacing exactly where they did
//! before; only the per-tick polling becomes free.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Wake, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
    /// Optional frontend readiness signal, called only after publishing state.
    waker: Option<Waker>,
    /// Position/cache updates are coalesced to the existing seekbar cadence.
    last_motion_notice: Option<Instant>,
    /// A ticket published with the last request actually processed by the worker.
    cache_export: Option<super::cache_export::PlaybackCacheHandle>,
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
    /// One coalesced readiness signal from an event-capable backend.
    Refresh,
    /// Load and start a media item.
    Play {
        /// Generation invalidated before this request entered the queue.
        epoch: u64,
        /// Requested media.
        input: Box<PlaybackInput>,
        /// Acknowledgement channel.
        reply: Sender<Result<()>>,
    },
    /// Apply a playback command.
    Command {
        /// Current generation, including any stop invalidation before queueing.
        epoch: u64,
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

/// Bounds queued backend readiness to one message even during an event burst.
struct BackendWake {
    pending: AtomicBool,
    jobs: Sender<Job>,
}

impl Wake for BackendWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.jobs.send(Job::Refresh);
        }
    }
}

/// A playback backend supervised on its own thread.
pub struct ThreadedBackend {
    /// Invalidates cached tickets even while a load/stop waits behind polling.
    cache_epoch: Arc<AtomicU64>,
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
    pub fn new<B>(mut backend: B) -> Self
    where
        B: PlaybackBackend + Send + 'static,
    {
        let process_id = backend.process_id();
        let (job_sender, job_receiver) = channel();
        let backend_wake = Arc::new(BackendWake {
            pending: AtomicBool::new(false),
            jobs: job_sender.clone(),
        });
        let event_driven = backend.set_worker_waker(Some(Waker::from(backend_wake.clone())));
        let (event_sender, event_receiver) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let cache_epoch = Arc::new(AtomicU64::new(0));
        let worker_epoch = Arc::clone(&cache_epoch);
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("youta-playback".to_owned())
            .spawn(move || {
                run(
                    backend,
                    &job_receiver,
                    &event_sender,
                    &worker_shared,
                    &worker_epoch,
                    &backend_wake,
                    event_driven,
                )
            })
            .ok();
        Self {
            cache_epoch,
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
    let mut semantic_previous = guard.status.clone();
    semantic_previous.position = status.position;
    semantic_previous
        .buffered_ranges
        .clone_from(&status.buffered_ranges);
    let notify = guard.status != status
        && (semantic_previous != status
            || guard
                .last_motion_notice
                .is_none_or(|last| last.elapsed() >= ACTIVE_REFRESH));
    guard.status = status;
    let waker = if notify {
        guard.last_motion_notice = Some(Instant::now());
        guard.waker.clone()
    } else {
        None
    };
    drop(guard);
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// Stores a failure, keeping the first one the reducer has not read yet.
fn publish_failure(shared: &Arc<Mutex<Shared>>, error: PlaybackError) {
    let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.failure.is_none() {
        guard.failure = Some(error);
        let waker = guard.waker.clone();
        drop(guard);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Notifies only after a lifecycle event was queued for the reducer.
fn wake_frontend(shared: &Arc<Mutex<Shared>>) {
    let waker = shared
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .waker
        .clone();
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// Supervises one backend until the handle is dropped or shutdown is requested.
fn run<B>(
    mut backend: B,
    jobs: &Receiver<Job>,
    events: &Sender<PlaybackEvent>,
    shared: &Arc<Mutex<Shared>>,
    cache_epoch: &Arc<AtomicU64>,
    backend_wake: &Arc<BackendWake>,
    event_driven: bool,
) where
    B: PlaybackBackend,
{
    let mut refresh = ACTIVE_REFRESH;
    let mut processed_epoch = 0;
    loop {
        let job = if event_driven {
            jobs.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            jobs.recv_timeout(refresh)
        };
        match job {
            Ok(Job::Refresh) => {
                backend_wake.pending.store(false, Ordering::Release);
            }
            Ok(Job::Play {
                input,
                reply,
                epoch,
            }) => {
                processed_epoch = epoch;
                let _ = reply.send(backend.play(&input));
            }
            Ok(Job::Command {
                command,
                reply,
                epoch,
            }) => {
                processed_epoch = epoch;
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

        let mut drained_events = 0;
        for _ in 0..MAX_EVENTS_PER_PASS {
            match backend.poll_event() {
                Ok(Some(event)) => {
                    if events.send(event).is_err() {
                        return;
                    }
                    wake_frontend(shared);
                    drained_events += 1;
                }
                Ok(None) => break,
                Err(error) => {
                    publish_failure(shared, error);
                    break;
                }
            }
        }
        if event_driven && drained_events == MAX_EVENTS_PER_PASS {
            // The final transport signal may already be consumed. Continue the
            // remaining lifecycle backlog without waiting for another event.
            backend_wake.wake_by_ref();
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
        let handle = backend
            .cache_export_handle()
            .map(|handle| handle.with_supervisor_epoch(Arc::clone(cache_epoch), processed_epoch));
        shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cache_export = handle;
    }
}

impl PlaybackBackend for ThreadedBackend {
    fn set_worker_waker(&mut self, waker: Option<Waker>) -> bool {
        let previous = {
            let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut shared.waker, waker)
        };
        drop(previous);
        wake_frontend(&self.shared);
        true
    }
    fn cache_export_handle(&self) -> Option<super::cache_export::PlaybackCacheHandle> {
        let handle = self.shared.try_lock().ok()?.cache_export.clone()?;
        handle.is_current().then_some(handle)
    }

    fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    fn play(&mut self, input: &PlaybackInput) -> Result<()> {
        let epoch = self
            .cache_epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let input = Box::new(input.clone());
        self.request(|reply| Job::Play {
            input,
            reply,
            epoch,
        })
    }

    fn command(&mut self, command: PlayerCommand) -> Result<()> {
        if matches!(
            command,
            PlayerCommand::Stop | PlayerCommand::ReleaseEndOfFile
        ) {
            self.cache_epoch.fetch_add(1, Ordering::AcqRel);
        }
        let epoch = self.cache_epoch.load(Ordering::Acquire);
        self.request(|reply| Job::Command {
            command,
            reply,
            epoch,
        })
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
        self.cache_epoch.fetch_add(1, Ordering::AcqRel);
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

    /// A paused event-capable backend needs no recurring status timer.
    #[test]
    fn notification_backend_sleeps_until_an_actual_change() {
        struct ReadyBackend {
            calls: Arc<AtomicUsize>,
            wake: Arc<Mutex<Option<Waker>>>,
        }
        impl PlaybackBackend for ReadyBackend {
            fn set_worker_waker(&mut self, waker: Option<Waker>) -> bool {
                *self.wake.lock().unwrap() = waker.clone();
                if let Some(waker) = waker {
                    waker.wake();
                }
                true
            }
            fn play(&mut self, _: &PlaybackInput) -> Result<()> {
                Ok(())
            }
            fn command(&mut self, _: PlayerCommand) -> Result<()> {
                Ok(())
            }
            fn status(&mut self) -> Result<PlaybackStatus> {
                self.calls.fetch_add(1, Ordering::AcqRel);
                Ok(PlaybackStatus::default())
            }
            fn shutdown(&mut self) -> Result<()> {
                Ok(())
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let wake = Arc::new(Mutex::new(None));
        let mut backend = ThreadedBackend::new(ReadyBackend {
            calls: calls.clone(),
            wake: wake.clone(),
        });
        assert!(wait_until(|| calls.load(Ordering::Acquire) == 1));
        thread::sleep(IDLE_REFRESH + Duration::from_millis(100));
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "paused observation has no heartbeat"
        );
        wake.lock().unwrap().as_ref().unwrap().wake_by_ref();
        assert!(wait_until(|| calls.load(Ordering::Acquire) == 2));
        backend.shutdown().unwrap();
    }

    /// A noisy IPC connection cannot grow the supervisor's wake queue without bound.
    #[test]
    fn backend_readiness_is_coalesced_until_consumed() {
        let (jobs, receiver) = channel();
        let wake = Arc::new(BackendWake {
            pending: AtomicBool::new(false),
            jobs,
        });
        for _ in 0..1000 {
            wake.wake_by_ref();
        }
        assert!(matches!(receiver.try_recv(), Ok(Job::Refresh)));
        assert!(receiver.try_recv().is_err());
        wake.pending.store(false, Ordering::Release);
        wake.wake_by_ref();
        assert!(matches!(receiver.try_recv(), Ok(Job::Refresh)));
    }

    /// A single readiness signal must carry a backlog across bounded drain passes.
    #[test]
    fn notification_backend_rearms_a_full_lifecycle_pass() {
        struct BurstBackend {
            remaining: usize,
        }
        impl PlaybackBackend for BurstBackend {
            fn set_worker_waker(&mut self, waker: Option<Waker>) -> bool {
                if let Some(waker) = waker {
                    waker.wake();
                }
                true
            }
            fn play(&mut self, _: &PlaybackInput) -> Result<()> {
                Ok(())
            }
            fn command(&mut self, _: PlayerCommand) -> Result<()> {
                Ok(())
            }
            fn status(&mut self) -> Result<PlaybackStatus> {
                Ok(PlaybackStatus::default())
            }
            fn poll_event(&mut self) -> Result<Option<PlaybackEvent>> {
                if self.remaining == 0 {
                    return Ok(None);
                }
                self.remaining -= 1;
                Ok(Some(PlaybackEvent::MediaLoaded))
            }
            fn shutdown(&mut self) -> Result<()> {
                Ok(())
            }
        }
        let count = MAX_EVENTS_PER_PASS + 7;
        let mut backend = ThreadedBackend::new(BurstBackend { remaining: count });
        for _ in 0..count {
            assert_eq!(
                backend
                    .events
                    .recv_timeout(Duration::from_secs(1))
                    .expect("all events arrive without another backend wake"),
                PlaybackEvent::MediaLoaded
            );
        }
        backend.shutdown().unwrap();
    }

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

    /// A backend paused inside status demonstrates that export lookup bypasses
    /// the synchronous command queue and queued loads revoke the previous ticket.
    #[test]
    fn cache_ticket_lookup_never_waits_for_polling_and_queued_load_revokes_it() {
        use super::super::cache_export::CacheExportControl;
        use std::sync::atomic::AtomicBool;

        struct CacheBackend {
            control: CacheExportControl,
            block: Arc<AtomicBool>,
            entered: Arc<AtomicBool>,
        }
        impl PlaybackBackend for CacheBackend {
            fn cache_export_handle(
                &self,
            ) -> Option<super::super::cache_export::PlaybackCacheHandle> {
                self.control.handle()
            }
            fn play(&mut self, input: &PlaybackInput) -> Result<()> {
                self.control.begin_load(&input.location);
                Ok(())
            }
            fn command(&mut self, _: PlayerCommand) -> Result<()> {
                Ok(())
            }
            fn status(&mut self) -> Result<PlaybackStatus> {
                if self.block.load(Ordering::Acquire) {
                    self.entered.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while self.block.load(Ordering::Acquire) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                Ok(PlaybackStatus {
                    idle: false,
                    ..PlaybackStatus::default()
                })
            }
            fn poll_event(&mut self) -> Result<Option<PlaybackEvent>> {
                Ok(None)
            }
            fn shutdown(&mut self) -> Result<()> {
                self.control.shutdown();
                Ok(())
            }
        }
        let control = CacheExportControl::new(
            123,
            "unused.sock".into(),
            "/tmp/unused-cache-fixture".into(),
        );
        control.begin_load("https://example.invalid/first.opus");
        control.loaded();
        let block = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let mut backend = ThreadedBackend::new(CacheBackend {
            control,
            block: Arc::clone(&block),
            entered: Arc::clone(&entered),
        });
        assert!(wait_until(|| backend.cache_export_handle().is_some()));
        let first = backend
            .cache_export_handle()
            .expect("published cache ticket");
        block.store(true, Ordering::Release);
        assert!(wait_until(|| entered.load(Ordering::Acquire)));
        let lookup = Instant::now();
        assert!(backend.cache_export_handle().is_some());
        assert!(lookup.elapsed() < Duration::from_millis(50));
        let epoch = Arc::clone(&backend.cache_epoch);
        let before = epoch.load(Ordering::Acquire);
        let worker = thread::spawn(move || {
            let input = PlaybackInput {
                location: "https://example.invalid/replacement.opus".to_owned(),
                start_at: Duration::ZERO,
                title: None,
                verify_remote_format: false,
                http_headers: super::super::PlaybackHttpHeaders::default(),
                bypass_ytdl: true,
                keep_open: false,
            };
            backend.play(&input).expect("queued load");
            backend
        });
        assert!(wait_until(|| epoch.load(Ordering::Acquire) != before));
        assert!(
            !first.is_current(),
            "revoke before the blocked worker sees the queued load"
        );
        block.store(false, Ordering::Release);
        let mut backend = worker.join().expect("load worker");
        assert!(
            backend.cache_export_handle().is_none(),
            "load acknowledgement is not a completed load"
        );
        backend.shutdown().expect("shutdown");
    }
}
