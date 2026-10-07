//! Shared frontend notification for already-published background work.
//!
//! This module owns no channel, timer, runtime, or thread. Frontends provide a
//! standard task waker; workers retain their existing payload queues and wake
//! only after publishing a result. Clones share registration across workers.

use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Wake, Waker};

/// A replaceable frontend wake-up shared by independent background workers.
#[derive(Clone, Default)]
pub struct WorkerNotifier {
    waker: Arc<Mutex<Option<Waker>>>,
}

impl WorkerNotifier {
    /// Installs a frontend and wakes once for results queued before attachment.
    /// Passing `None` detaches it without consuming or cancelling queued work.
    pub fn set_waker(&self, waker: Option<Waker>) {
        let previous = {
            let mut registered = self.waker.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *registered, waker)
        };
        // Custom waker callbacks and destructors never run under our mutex.
        drop(previous);
        self.wake();
    }

    /// Notifies the frontend after a result or a channel disconnect is visible.
    pub fn wake(&self) {
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Creates a final notification for completion or panic unwinding.
    /// Declare the guard before a sender so the sender drops before this guard.
    #[must_use]
    pub fn completion_guard(&self) -> WorkerCompletionGuard {
        WorkerCompletionGuard(self.clone())
    }

    /// Creates a stable relay that follows future frontend registrations.
    #[must_use]
    pub fn as_waker(&self) -> Waker {
        Waker::from(Arc::new(self.clone()))
    }
}

impl Wake for WorkerNotifier {
    fn wake(self: Arc<Self>) {
        WorkerNotifier::wake(&self);
    }
}

/// Wakes a frontend when a worker scope finishes, including unwinding.
#[must_use]
pub struct WorkerCompletionGuard(WorkerNotifier);

impl Drop for WorkerCompletionGuard {
    fn drop(&mut self) {
        self.0.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Existing workers share replacement/detachment, including initial replay.
    #[test]
    fn clones_share_registration_and_replay_without_consuming_work() {
        let notifier = WorkerNotifier::default();
        let worker = notifier.clone();
        worker.wake();
        let first = Arc::new(Count::default());
        notifier.set_waker(Some(Waker::from(first.clone())));
        assert_eq!(first.0.load(Ordering::SeqCst), 1);
        worker.wake();
        assert_eq!(first.0.load(Ordering::SeqCst), 2);
        let replacement = Arc::new(Count::default());
        notifier.set_waker(Some(Waker::from(replacement.clone())));
        worker.wake();
        assert_eq!(first.0.load(Ordering::SeqCst), 2);
        assert_eq!(replacement.0.load(Ordering::SeqCst), 2);
        notifier.set_waker(None);
        worker.wake();
        assert_eq!(replacement.0.load(Ordering::SeqCst), 2);
    }

    /// Completion guards notify even when a worker exits without a normal result.
    #[test]
    fn completion_guard_wakes_during_unwind() {
        let notifier = WorkerNotifier::default();
        let count = Arc::new(Count::default());
        notifier.set_waker(Some(Waker::from(count.clone())));
        let worker = std::thread::spawn(move || {
            let _completion = notifier.completion_guard();
            panic!("simulated worker failure");
        });
        assert!(worker.join().is_err());
        assert_eq!(count.0.load(Ordering::SeqCst), 2);
    }
}
