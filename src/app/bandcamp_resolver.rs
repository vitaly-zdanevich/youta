//! Latest-only Bandcamp playback resolution, independent of controller UI state.
//!
//! This owner keeps the resolver, worker channels, generation, and pending context
//! together. Only media and audio format cross the worker boundary; the controller
//! retains responsibility for validating the UI selection and starting playback.

use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use crate::config::BandcampAudioFormat;
use crate::playback::Result as PlaybackResult;
use crate::providers::bandcamp::{
    BandcampMediaUrl, BandcampResolution, BandcampResolvePurpose, BandcampResolver,
};

/// Resolve operation used by the bounded Bandcamp playback worker.
pub(super) trait BandcampResolveClient: Send {
    /// Resolves one canonical release only after an explicit user action.
    fn resolve(
        &self,
        source: &BandcampMediaUrl,
        format: BandcampAudioFormat,
        purpose: BandcampResolvePurpose,
    ) -> PlaybackResult<BandcampResolution>;
}

impl BandcampResolveClient for BandcampResolver {
    fn resolve(
        &self,
        source: &BandcampMediaUrl,
        format: BandcampAudioFormat,
        purpose: BandcampResolvePurpose,
    ) -> PlaybackResult<BandcampResolution> {
        BandcampResolver::resolve(self, source, format, purpose)
    }
}

/// Why an explicit action was not accepted; UI wording belongs to the controller.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum SubmitError {
    /// Worker startup failed, or this owner has already shut down.
    Unavailable,
    /// The bounded worker request channel did not accept the selected release.
    NotAccepted,
}

/// Latest matching result and its controller-owned context, consumed exactly once.
pub(super) struct OwnedCompletion<C> {
    pub(super) context: C,
    pub(super) result: PlaybackResult<BandcampResolution>,
}

/// Sole lifecycle owner of one active and one replaceable queued resolve action.
///
/// Context never enters the worker and need not be `Send`. Cancellation revokes
/// completion ownership, not the bounded resolver operation itself. Shutdown and
/// `Drop` wait for that operation before releasing the worker and signed results.
pub(super) struct BandcampResolverOwner<C> {
    lifecycle: Lifecycle,
    generation: u64,
    pending: Option<Pending<C>>,
}

/// Complete lifecycle states prevent partially initialized channel combinations.
enum Lifecycle {
    Dormant(Box<dyn BandcampResolveClient>),
    Running(Worker),
    Stopped,
}

/// Handles that must be created and released together for a running worker.
struct Worker {
    requests: Sender<Command>,
    request_drain: Receiver<Command>,
    responses: Receiver<Completion>,
    thread: JoinHandle<()>,
}

/// Opaque context corresponding to the latest accepted request generation.
struct Pending<C> {
    generation: u64,
    context: C,
}

/// Action-authorized work; signed URLs appear only in the response channel.
enum Command {
    Resolve {
        generation: u64,
        media: BandcampMediaUrl,
        format: BandcampAudioFormat,
    },
    Shutdown,
}

/// URL-bearing result awaiting generation validation on the controller thread.
struct Completion {
    generation: u64,
    result: PlaybackResult<BandcampResolution>,
}

impl<C> BandcampResolverOwner<C> {
    /// Retains a resolver without spawning a thread or making a provider request.
    pub(super) fn new(resolver: Box<dyn BandcampResolveClient>) -> Self {
        Self {
            lifecycle: Lifecycle::Dormant(resolver),
            generation: 0,
            pending: None,
        }
    }

    /// Starts lazily and replaces queued obsolete work after an explicit action.
    ///
    /// The previous pending context is replaced only once submission succeeds.
    pub(super) fn request(
        &mut self,
        media: BandcampMediaUrl,
        format: BandcampAudioFormat,
        context: C,
    ) -> Result<(), SubmitError> {
        self.ensure_worker()?;
        let Lifecycle::Running(worker) = &self.lifecycle else {
            return Err(SubmitError::Unavailable);
        };
        self.generation = self.generation.wrapping_add(1);
        while worker.request_drain.try_recv().is_ok() {}
        worker
            .requests
            .try_send(Command::Resolve {
                generation: self.generation,
                media,
                format,
            })
            .map_err(|_| SubmitError::NotAccepted)?;
        self.pending = Some(Pending {
            generation: self.generation,
            context,
        });
        Ok(())
    }

    /// Returns the current completion once, discarding stale results without waiting.
    pub(super) fn poll(&mut self) -> Option<OwnedCompletion<C>> {
        let Lifecycle::Running(worker) = &self.lifecycle else {
            return None;
        };
        while let Ok(completion) = worker.responses.try_recv() {
            if self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.generation == completion.generation)
            {
                let pending = self
                    .pending
                    .take()
                    .expect("matching generation owns context");
                return Some(OwnedCompletion {
                    context: pending.context,
                    result: completion.result,
                });
            }
        }
        None
    }

    /// Revokes pending ownership and reports whether the UI activity should clear.
    pub(super) fn cancel(&mut self) -> bool {
        if self.pending.take().is_none() {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// Discards queued work and joins any active bounded resolve, at most once.
    ///
    /// A stopped owner cannot restart. Cancellation does not terminate provider
    /// subprocesses; their existing timeout/output limits still bound the join.
    pub(super) fn shutdown(&mut self) {
        self.pending = None;
        self.generation = self.generation.wrapping_add(1);
        let lifecycle = std::mem::replace(&mut self.lifecycle, Lifecycle::Stopped);
        if let Lifecycle::Running(worker) = lifecycle {
            while worker.request_drain.try_recv().is_ok() {}
            let _ = worker.requests.send(Command::Shutdown);
            drop(worker.request_drain);
            drop(worker.responses);
            let _ = worker.thread.join();
        }
    }

    /// Creates both bounded channels and their sole resolver thread together.
    fn ensure_worker(&mut self) -> Result<(), SubmitError> {
        if matches!(self.lifecycle, Lifecycle::Running(_)) {
            return Ok(());
        }
        let Lifecycle::Dormant(resolver) =
            std::mem::replace(&mut self.lifecycle, Lifecycle::Stopped)
        else {
            return Err(SubmitError::Unavailable);
        };
        let (request_sender, request_receiver) = bounded(1);
        let request_drain = request_receiver.clone();
        let (response_sender, response_receiver) = bounded(1);
        let response_drain = response_receiver.clone();
        let thread = thread::Builder::new()
            .name("youta-bandcamp-resolver".to_owned())
            .spawn(move || worker_loop(request_receiver, response_sender, response_drain, resolver))
            .map_err(|_| SubmitError::Unavailable)?;
        self.lifecycle = Lifecycle::Running(Worker {
            requests: request_sender,
            request_drain,
            responses: response_receiver,
            thread,
        });
        Ok(())
    }
}

impl<C> Drop for BandcampResolverOwner<C> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Resolves at most one active and one queued action, keeping only the newest result.
fn worker_loop(
    requests: Receiver<Command>,
    responses: Sender<Completion>,
    response_drain: Receiver<Completion>,
    resolver: Box<dyn BandcampResolveClient>,
) {
    while let Ok(command) = requests.recv() {
        let Command::Resolve {
            generation,
            media,
            format,
        } = command
        else {
            break;
        };
        let mut completion = Completion {
            generation,
            result: resolver.resolve(&media, format, BandcampResolvePurpose::Playback),
        };
        loop {
            match responses.try_send(completion) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    completion = returned;
                    let _ = response_drain.try_recv();
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use crate::playback::PlaybackError;
    use crossbeam_channel::unbounded;

    /// Readiness guard for overloaded CI, not a worker latency requirement.
    const READY_TIMEOUT: Duration = Duration::from_secs(5);

    /// A worker call held until the test explicitly chooses its outcome.
    struct ControlledCall {
        media: BandcampMediaUrl,
        format: BandcampAudioFormat,
        purpose: BandcampResolvePurpose,
        release: Option<Sender<bool>>,
    }

    impl ControlledCall {
        /// Releases the resolver with either a fixture result or a fixture error.
        fn finish(mut self, success: bool) {
            self.release
                .take()
                .expect("unreleased call")
                .send(success)
                .expect("live resolver");
        }
    }

    impl Drop for ControlledCall {
        fn drop(&mut self) {
            // A failed assertion must not leave owner shutdown waiting on this fake.
            if let Some(release) = self.release.take() {
                let _ = release.try_send(true);
            }
        }
    }

    /// Network-free resolver whose lifetime and active call are observable.
    struct ControlledResolver {
        started: Sender<ControlledCall>,
        dropped: Arc<AtomicBool>,
    }

    impl BandcampResolveClient for ControlledResolver {
        fn resolve(
            &self,
            source: &BandcampMediaUrl,
            format: BandcampAudioFormat,
            purpose: BandcampResolvePurpose,
        ) -> PlaybackResult<BandcampResolution> {
            let (release, released) = bounded(1);
            self.started
                .send(ControlledCall {
                    media: source.clone(),
                    format,
                    purpose,
                    release: Some(release),
                })
                .expect("test observes resolver calls");
            if !released
                .recv_timeout(READY_TIMEOUT)
                .expect("test releases resolver")
            {
                return Err(PlaybackError::Protocol(
                    "controlled Bandcamp failure".to_owned(),
                ));
            }
            Ok(BandcampResolution {
                source: source.clone(),
                purpose,
                format,
                tracks: Vec::new(),
                possibly_truncated: false,
            })
        }
    }

    impl Drop for ControlledResolver {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    /// Builds a dormant owner and separate handles for controlling its worker.
    fn controlled_owner<C>() -> (
        BandcampResolverOwner<C>,
        Receiver<ControlledCall>,
        Arc<AtomicBool>,
    ) {
        let (started, calls) = unbounded();
        let dropped = Arc::new(AtomicBool::new(false));
        let owner = BandcampResolverOwner::new(Box::new(ControlledResolver {
            started,
            dropped: Arc::clone(&dropped),
        }));
        (owner, calls, dropped)
    }

    /// Creates a stable page locator without consulting Bandcamp.
    fn media(slug: &str) -> BandcampMediaUrl {
        BandcampMediaUrl::parse(
            format!("https://fixture-artist.bandcamp.com/track/{slug}")
                .parse()
                .expect("fixture URL"),
        )
        .expect("canonical fixture page")
    }

    /// Waits only in tests; production owner polling is always nonblocking.
    fn completion<C>(owner: &mut BandcampResolverOwner<C>) -> OwnedCompletion<C> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(completion) = owner.poll() {
                return completion;
            }
            assert!(Instant::now() < deadline, "resolver completion timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn bandcamp_owner_is_lazy_and_shutdown_is_terminal_and_idempotent() {
        let (mut owner, calls, dropped) = controlled_owner::<()>();
        assert!(matches!(owner.lifecycle, Lifecycle::Dormant(_)));
        assert!(calls.is_empty());
        assert!(!owner.cancel());
        assert!(owner.poll().is_none());
        owner.shutdown();
        owner.shutdown();
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(
            owner.request(media("stopped"), BandcampAudioFormat::BestAvailable, ()),
            Err(SubmitError::Unavailable),
        );
        assert!(!owner.cancel());
    }

    #[test]
    fn bandcamp_owner_replaces_queued_work_and_keeps_non_send_context_local() {
        let (mut owner, calls, _) = controlled_owner();
        let first_context = Rc::new(Cell::new(1));
        owner
            .request(
                media("first"),
                BandcampAudioFormat::BestAvailable,
                first_context,
            )
            .expect("first accepted");
        let first = calls
            .recv_timeout(READY_TIMEOUT)
            .expect("active first call");
        owner
            .request(
                media("obsolete"),
                BandcampAudioFormat::BestAvailable,
                Rc::new(Cell::new(2)),
            )
            .expect("queued second accepted");
        let context = Rc::new(Cell::new(3));
        owner
            .request(
                media("latest"),
                BandcampAudioFormat::BestAvailable,
                Rc::clone(&context),
            )
            .expect("replacement accepted");
        first.finish(true);
        let latest = calls
            .recv_timeout(READY_TIMEOUT)
            .expect("latest queued call");
        assert_eq!(latest.media, media("latest"));
        assert_eq!(latest.format, BandcampAudioFormat::BestAvailable);
        assert_eq!(latest.purpose, BandcampResolvePurpose::Playback);
        latest.finish(true);
        let completed = completion(&mut owner);
        assert!(Rc::ptr_eq(&completed.context, &context));
        assert_eq!(
            completed.result.expect("latest success").source,
            media("latest")
        );
        assert!(owner.poll().is_none());
        owner.shutdown();
        assert!(calls.is_empty());
    }

    #[test]
    fn bandcamp_owner_cancellation_discards_stale_success_and_failure() {
        for success in [true, false] {
            let (mut owner, calls, _) = controlled_owner();
            owner
                .request(
                    media("cancelled"),
                    BandcampAudioFormat::BestAvailable,
                    "cancelled",
                )
                .expect("first accepted");
            let cancelled = calls.recv_timeout(READY_TIMEOUT).expect("active call");
            assert!(owner.cancel());
            assert!(!owner.cancel());
            assert!(owner.poll().is_none());
            owner
                .request(
                    media("current"),
                    BandcampAudioFormat::BestAvailable,
                    "current",
                )
                .expect("new selection accepted");
            cancelled.finish(success);
            let current = calls
                .recv_timeout(READY_TIMEOUT)
                .expect("new selection call");
            // The obsolete response exists while the current resolver remains blocked.
            assert!(owner.poll().is_none());
            current.finish(true);
            let completed = completion(&mut owner);
            assert_eq!(completed.context, "current");
            assert!(completed.result.is_ok());
            assert!(owner.poll().is_none());
        }
    }

    #[test]
    fn bandcamp_owner_delivers_current_failure_once() {
        let (mut owner, calls, _) = controlled_owner();
        owner
            .request(media("failure"), BandcampAudioFormat::BestAvailable, 7)
            .expect("request accepted");
        calls
            .recv_timeout(READY_TIMEOUT)
            .expect("active call")
            .finish(false);
        let completed = completion(&mut owner);
        assert_eq!(completed.context, 7);
        assert!(
            completed
                .result
                .expect_err("fixture failure")
                .to_string()
                .contains("controlled Bandcamp failure")
        );
        assert!(!owner.cancel());
        assert!(owner.poll().is_none());
    }

    #[test]
    fn bandcamp_owner_shutdown_and_drop_join_an_active_worker() {
        for explicit_shutdown in [true, false] {
            let (mut owner, calls, dropped) = controlled_owner::<()>();
            owner
                .request(media("active"), BandcampAudioFormat::BestAvailable, ())
                .expect("request accepted");
            let active = calls.recv_timeout(READY_TIMEOUT).expect("active call");
            let (stopping, started_stop) = bounded(1);
            let shutdown = thread::spawn(move || {
                stopping.send(()).expect("shutdown observer");
                if explicit_shutdown {
                    owner.shutdown();
                    owner.shutdown();
                    assert!(owner.poll().is_none());
                    assert!(!owner.cancel());
                }
                drop(owner);
            });
            started_stop
                .recv_timeout(READY_TIMEOUT)
                .expect("shutdown started");
            assert!(!dropped.load(Ordering::SeqCst));
            active.finish(true);
            shutdown.join().expect("owner shutdown joined");
            assert!(dropped.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn bandcamp_worker_replaces_unconsumed_completion_instead_of_blocking() {
        let (mut owner, calls, _) = controlled_owner::<()>();
        owner
            .request(media("first"), BandcampAudioFormat::BestAvailable, ())
            .expect("first accepted");
        calls
            .recv_timeout(READY_TIMEOUT)
            .expect("first call")
            .finish(true);
        owner
            .request(media("latest"), BandcampAudioFormat::BestAvailable, ())
            .expect("second accepted");
        let latest = calls.recv_timeout(READY_TIMEOUT).expect("second call");
        let Lifecycle::Running(worker) = &owner.lifecycle else {
            panic!("started worker")
        };
        assert_eq!(worker.responses.len(), 1);
        latest.finish(true);
        // Join after both calls without consuming either response, proving the bound
        // cannot deadlock resolution while the controller is busy with another action.
        let Lifecycle::Running(worker) =
            std::mem::replace(&mut owner.lifecycle, Lifecycle::Stopped)
        else {
            panic!("started worker")
        };
        worker
            .requests
            .send(Command::Shutdown)
            .expect("shutdown queued");
        worker.thread.join().expect("worker joined without polling");
        assert_eq!(worker.responses.len(), 1);
        let completed = worker.responses.try_recv().expect("latest completion");
        assert_eq!(
            completed.result.expect("latest success").source,
            media("latest")
        );
    }
}
