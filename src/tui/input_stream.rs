//! Wake-driven Crossterm input with an explicit foreground-editor handoff.
//!
//! Crossterm wakes its private input thread when `EventStream` is dropped, but
//! does not join it. Every scheduled read owns the task waker until that read
//! has stopped. Short-lived waker leases therefore let us await the final read
//! without depending on private Crossterm fields or racing an editor for input.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};

use crossterm::event::{Event, EventStream};
use futures_core::Stream;

/// A terminal stream whose outstanding reads stop before terminal ownership changes.
pub(super) struct TerminalEventStream {
    stream: Option<EventStream>,
    wake: Arc<mio::Waker>,
    readers: Arc<ReaderLeases>,
}

impl TerminalEventStream {
    /// Creates terminal input whose ready events wake the frontend's existing poll.
    pub(super) fn new(wake: Arc<mio::Waker>) -> Self {
        Self {
            stream: Some(EventStream::new()),
            wake,
            readers: Arc::new(ReaderLeases::default()),
        }
    }

    /// Reads a ready event or arms one library-owned background read.
    pub(super) fn poll_next(&mut self) -> Poll<Option<io::Result<Event>>> {
        let Some(stream) = self.stream.as_mut() else {
            return Poll::Pending;
        };
        // Do not retain this waker in the frontend. Only a scheduled Crossterm
        // task may keep its lease after this call, including a queued task that
        // has not started polling the terminal yet.
        let waker = ReaderWake::waker(self.wake.clone(), self.readers.clone());
        let mut context = Context::from_waker(&waker);
        Pin::new(stream).poll_next(&mut context)
    }

    /// Stops input and awaits all library-owned reads before an editor or shell runs.
    ///
    /// A timeout would allow a lingering reader to consume the editor's keys, so
    /// this barrier never treats a missed deadline as a successful handoff.
    pub(super) fn suspend(&mut self) {
        stop_stream(&mut self.stream, &self.readers);
    }

    /// Recreates input after the previous reader has surrendered the terminal.
    #[cfg(feature = "local-browser")]
    pub(super) fn resume(&mut self) {
        if self.stream.is_none() {
            self.readers.wait();
            self.stream = Some(EventStream::new());
        }
    }
}

impl Drop for TerminalEventStream {
    fn drop(&mut self) {
        self.suspend();
    }
}

/// Drops the source before waiting, allowing its cancellation to wake active reads.
///
/// Keeping the source generic makes the shutdown ordering testable without
/// touching Crossterm's process-global real terminal reader.
fn stop_stream<T>(stream: &mut Option<T>, readers: &ReaderLeases) {
    drop(stream.take());
    readers.wait();
}

/// Tracks wakers owned by queued or active terminal read tasks, not notifications.
#[derive(Default)]
struct ReaderLeases {
    active: Mutex<usize>,
    finished: Condvar,
}

impl ReaderLeases {
    /// Acquires one lease before a waker becomes visible to the stream.
    fn acquire(&self) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        *active += 1;
    }

    /// Releases a lease only after the last clone of its task waker disappears.
    fn release(&self) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        *active -= 1;
        if *active == 0 {
            self.finished.notify_all();
        }
    }

    /// Waits until neither a queued task nor an active read can touch terminal input.
    fn wait(&self) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        while *active != 0 {
            active = self
                .finished
                .wait(active)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Forwards readiness while retaining a lease through the task's final waker drop.
struct ReaderWake {
    wake: Arc<mio::Waker>,
    readers: Arc<ReaderLeases>,
}

impl ReaderWake {
    /// Creates a fresh lease for one `poll_next` invocation.
    fn waker(wake: Arc<mio::Waker>, readers: Arc<ReaderLeases>) -> Waker {
        readers.acquire();
        Waker::from(Arc::new(Self { wake, readers }))
    }
}

impl Wake for ReaderWake {
    fn wake(self: Arc<Self>) {
        let _ = self.wake.wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let _ = self.wake.wake();
    }
}

impl Drop for ReaderWake {
    fn drop(&mut self) {
        self.readers.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;
    use std::time::Duration;

    /// Supplies real notification plumbing without starting a process-global input reader.
    fn fixture() -> (mio::Poll, Arc<mio::Waker>, Arc<ReaderLeases>) {
        let poll = mio::Poll::new().expect("notification poll");
        let wake = Arc::new(mio::Waker::new(poll.registry(), mio::Token(0)).expect("notifier"));
        (poll, wake, Arc::new(ReaderLeases::default()))
    }

    /// An immediate stream result retains no background task or shutdown work.
    #[test]
    fn unretained_poll_wakers_leave_no_reader_lease() {
        let (_poll, wake, readers) = fixture();
        let waker = ReaderWake::waker(wake, readers.clone());
        assert_eq!(*readers.active.lock().expect("active leases"), 1);
        drop(waker);
        assert_eq!(*readers.active.lock().expect("active leases"), 0);
        readers.wait();
    }

    /// Waking is not cancellation: all task-owned clones must disappear before handoff.
    #[test]
    fn suspension_waits_for_the_final_task_waker_clone() {
        let (_poll, wake, readers) = fixture();
        let waker = ReaderWake::waker(wake.clone(), readers.clone());
        let task_waker = waker.clone();
        let retained_task_clone = task_waker.clone();
        drop(waker);
        let (completed, completion) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut input = TerminalEventStream {
                stream: None,
                wake,
                readers,
            };
            input.suspend();
            completed.send(()).expect("handoff completion");
        });
        assert_eq!(
            completion.recv_timeout(Duration::from_millis(25)),
            Err(RecvTimeoutError::Timeout),
            "a queued reader still owns its waker"
        );
        task_waker.wake();
        retained_task_clone.wake_by_ref();
        assert_eq!(
            completion.recv_timeout(Duration::from_millis(25)),
            Err(RecvTimeoutError::Timeout),
            "a readiness notification must not surrender the reader's remaining clone"
        );
        drop(retained_task_clone);
        completion
            .recv_timeout(Duration::from_secs(2))
            .expect("reader released terminal");
        handle.join().expect("suspension thread");
    }

    /// Cancellation must drop its source before waiting for the reader that it wakes.
    #[test]
    fn suspension_drops_the_stream_before_waiting_for_readers() {
        struct Source(mpsc::Sender<()>);
        impl Drop for Source {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let (_poll, wake, readers) = fixture();
        let task_waker = ReaderWake::waker(wake, readers.clone());
        let (dropped, drop_notice) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut stream = Some(Source(dropped));
            stop_stream(&mut stream, &readers);
            assert!(stream.is_none());
            stop_stream(&mut stream, &readers);
            completed.send(()).expect("handoff completion");
        });
        drop_notice
            .recv_timeout(Duration::from_secs(2))
            .expect("cancelled source");
        assert_eq!(
            completion.recv_timeout(Duration::from_millis(25)),
            Err(RecvTimeoutError::Timeout),
            "stream cancellation alone must not race the reader"
        );
        drop(task_waker);
        completion
            .recv_timeout(Duration::from_secs(2))
            .expect("reader released terminal");
        handle.join().expect("suspension thread");
    }
}
