//! One blocking readiness wait for terminal input, GPM, and worker replies.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Poll as TaskPoll, Wake, Waker};
use std::time::{Duration, Instant};

use crossterm::event::Event;
use mio::{Events, Poll, Token};

use super::input_stream::TerminalEventStream;
use super::{ConsolePointerAvailability, WaitOutcome, gpm_reconnect_needed};
#[cfg(all(feature = "gpm", target_os = "linux"))]
use crate::gpm::LinuxConsoleInput;

const WAKE_TOKEN: Token = Token(0);
#[cfg(all(feature = "gpm", target_os = "linux"))]
const GPM_TOKEN: Token = Token(1);

/// A coalesced signal, never a second consumer of the controller's response queue.
struct WorkerWake {
    ready: AtomicBool,
    wake: Arc<mio::Waker>,
}

impl Wake for WorkerWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.ready.swap(true, Ordering::AcqRel) {
            let _ = self.wake.wake();
        }
    }
}

/// Shares one OS wake handle between the input stream and worker completions.
struct InputReadiness {
    poll: Poll,
    events: Events,
    wake: Arc<mio::Waker>,
    worker: Arc<WorkerWake>,
}

impl InputReadiness {
    /// Creates the single OS registry and wake handle shared by both producers.
    fn new() -> io::Result<Self> {
        let poll = Poll::new()?;
        let wake = Arc::new(mio::Waker::new(poll.registry(), WAKE_TOKEN)?);
        Ok(Self {
            poll,
            events: Events::with_capacity(8),
            worker: Arc::new(WorkerWake {
                ready: AtomicBool::new(false),
                wake: wake.clone(),
            }),
            wake,
        })
    }

    /// Clones the notification handle without cloning any response receiver.
    fn worker_waker(&self) -> Waker {
        Waker::from(self.worker.clone())
    }

    /// Acknowledges a batch while retaining a later concurrent notification.
    fn take_worker_ready(&self) -> bool {
        self.worker.ready.swap(false, Ordering::AcqRel)
    }

    /// Blocks in the OS until readiness or the caller's animation deadline.
    fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        self.events.clear();
        self.poll.poll(&mut self.events, Some(timeout))
    }
}

/// Input events and worker replies interrupt the same portable blocking wait.
///
/// Crossterm owns decoding and resize notifications. The optional Linux GPM
/// adapter registers only its mouse socket; it never races a second reader for
/// keyboard input. Dropping `stream` first waits for its reader to stop before
/// the terminal session can restore cooked mode.
pub(super) struct TerminalInput {
    stream: TerminalEventStream,
    readiness: InputReadiness,
    pending: VecDeque<Event>,
    #[cfg(all(feature = "gpm", target_os = "linux"))]
    linux_console: Option<LinuxConsoleInput>,
}

impl TerminalInput {
    /// Connects portable terminal input and, when available, the GPM mouse socket.
    pub(super) fn new() -> io::Result<Self> {
        let readiness = InputReadiness::new()?;
        Ok(Self {
            stream: TerminalEventStream::new(readiness.wake.clone()),
            #[cfg(all(feature = "gpm", target_os = "linux"))]
            linux_console: LinuxConsoleInput::try_current(readiness.poll.registry(), GPM_TOKEN),
            readiness,
            pending: VecDeque::new(),
        })
    }

    /// Registers a wake-only handle; the controller retains ownership of replies.
    pub(super) fn worker_waker(&self) -> Waker {
        self.readiness.worker_waker()
    }

    /// Waits until input, a worker completion, or the ordinary animation deadline.
    pub(super) fn poll(&mut self, timeout: Duration) -> io::Result<WaitOutcome> {
        let started = Instant::now();
        let mut waited = false;
        loop {
            if !self.pending.is_empty() {
                return Ok(WaitOutcome::TerminalEvent);
            }
            match self.stream.poll_next() {
                TaskPoll::Ready(Some(event)) => {
                    self.pending.push_back(event?);
                    return Ok(WaitOutcome::TerminalEvent);
                }
                TaskPoll::Ready(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal input closed",
                    ));
                }
                TaskPoll::Pending => {}
            }
            if self.readiness.take_worker_ready() {
                return Ok(WaitOutcome::WorkerReady);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if waited && remaining.is_zero() {
                return Ok(WaitOutcome::Timeout);
            }
            waited = true;
            if let Err(error) = self.readiness.wait(remaining) {
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            #[cfg(all(feature = "gpm", target_os = "linux"))]
            if self
                .readiness
                .events
                .iter()
                .any(|event| event.token() == GPM_TOKEN)
                && let Some(input) = self.linux_console.as_mut()
            {
                let result = input.drain_ready();
                while let Some(event) = input.read() {
                    self.pending.push_back(event);
                }
                if result.is_err() {
                    // Keep any final decoded packets, then retain keyboard input.
                    // The next explicit F8 press can reconnect the optional socket.
                    self.linux_console = None;
                }
            }
        }
    }

    /// Takes only an event already decoded by `poll`; never blocks a second time.
    pub(super) fn read(&mut self) -> io::Result<Event> {
        self.pending
            .pop_front()
            .ok_or_else(|| io::ErrorKind::WouldBlock.into())
    }

    /// Stops terminal reads before a foreground editor inherits the same TTY.
    #[cfg(feature = "local-browser")]
    pub(super) fn suspend(&mut self) {
        self.stream.suspend();
    }

    /// Re-arms terminal input only after the editor has returned to Youta.
    #[cfg(feature = "local-browser")]
    pub(super) fn resume(&mut self) {
        self.stream.resume();
    }

    /// Reports whether this build includes the Linux GPM input adapter.
    pub(super) const fn gpm_supported() -> bool {
        cfg!(all(feature = "gpm", target_os = "linux"))
    }

    /// Reports whether the optional daemon connection is currently usable.
    pub(super) fn gpm_connected(&self) -> bool {
        #[cfg(all(feature = "gpm", target_os = "linux"))]
        return self.linux_console.is_some();
        #[cfg(not(all(feature = "gpm", target_os = "linux")))]
        false
    }

    /// Retries optional mouse input only on an explicit F8 press on a console.
    pub(super) fn retry_gpm_on_f8_press(
        &mut self,
        pressed: bool,
        physical_linux_console: bool,
    ) -> bool {
        if !gpm_reconnect_needed(
            pressed,
            ConsolePointerAvailability {
                physical_linux_console,
                gpm_supported: Self::gpm_supported(),
                gpm_connected: self.gpm_connected(),
                openrc_managed: false,
            },
        ) {
            return false;
        }
        #[cfg(all(feature = "gpm", target_os = "linux"))]
        return super::retry_optional_input_with(&mut self.linux_console, || {
            LinuxConsoleInput::try_current(self.readiness.poll.registry(), GPM_TOKEN)
        });
        #[cfg(not(all(feature = "gpm", target_os = "linux")))]
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// Finishing before a wait is safe, and a batch consumes only one notification.
    #[test]
    fn completion_before_wait_is_retained_and_coalesced() {
        let mut readiness = InputReadiness::new().unwrap();
        let waker = readiness.worker_waker();
        for _ in 0..10 {
            waker.wake_by_ref();
        }
        assert!(readiness.take_worker_ready());
        assert!(!readiness.take_worker_ready());
        readiness.wait(Duration::ZERO).unwrap();
        assert!(
            !readiness.events.is_empty(),
            "the OS wait also retains its wake"
        );
        readiness.wait(Duration::ZERO).unwrap();
        assert!(
            readiness.events.is_empty(),
            "no recurring timer or wake storm"
        );
        waker.wake_by_ref();
        assert!(
            readiness.take_worker_ready(),
            "a later completion re-arms the signal"
        );
    }

    /// A filesystem worker interrupts a long idle deadline without terminal keys.
    #[test]
    fn completion_wakes_a_blocking_input_wait() {
        let mut readiness = InputReadiness::new().unwrap();
        let waker = readiness.worker_waker();
        let (started, start) = mpsc::channel();
        let (done, complete) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started.send(()).unwrap();
            readiness.wait(Duration::from_secs(10)).unwrap();
            done.send(readiness.take_worker_ready()).unwrap();
        });
        start.recv().unwrap();
        waker.wake();
        assert!(complete.recv_timeout(Duration::from_secs(2)).unwrap());
        waiter.join().unwrap();
    }

    /// Terminal readiness never pretends to be a filesystem completion.
    #[test]
    fn terminal_wake_and_idle_wait_do_not_invent_worker_replies() {
        let mut readiness = InputReadiness::new().unwrap();
        readiness.wake.wake().unwrap();
        readiness.wait(Duration::ZERO).unwrap();
        assert!(!readiness.take_worker_ready());
        readiness.wait(Duration::ZERO).unwrap();
        assert!(readiness.events.is_empty());
        assert!(!readiness.take_worker_ready());
    }
}
