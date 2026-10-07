//! Coalesced completion messages for the desktop reducer's existing inbox.
//!
//! Kept independent of the window runtime so queue races and shutdown can be
//! tested without native desktop libraries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Weak};
use std::task::Wake;

/// A weak sender preserves inbox disconnection when the frontend is dropped.
pub(crate) struct InboxWake<T> {
    queued: AtomicBool,
    actions: Weak<Sender<T>>,
    message: fn() -> T,
}

impl<T> InboxWake<T> {
    /// Builds a signal for an existing inbox without retaining its sender lifetime.
    pub(crate) fn new(actions: &Arc<Sender<T>>, message: fn() -> T) -> Self {
        Self {
            queued: AtomicBool::new(false),
            actions: Arc::downgrade(actions),
            message,
        }
    }

    /// Rearms before draining results so concurrent completions cannot be lost.
    pub(crate) fn acknowledge(&self) {
        self.queued.store(false, Ordering::Release);
    }
}

impl<T: Send> Wake for InboxWake<T> {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.queued.swap(true, Ordering::AcqRel)
            && let Some(actions) = self.actions.upgrade()
        {
            let _ = actions.send((self.message)());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{TryRecvError, channel};
    use std::task::Waker;

    /// A burst is one inbox message, while a later completion reawakens reduction.
    #[test]
    fn completion_bursts_coalesce_and_rearm() {
        let (sender, inbox) = channel();
        let sender = Arc::new(sender);
        let wake = Arc::new(InboxWake::new(&sender, || 1));
        let waker = Waker::from(wake.clone());
        for _ in 0..1000 {
            waker.wake_by_ref();
        }
        assert_eq!(inbox.try_recv(), Ok(1));
        assert_eq!(inbox.try_recv(), Err(TryRecvError::Empty));
        wake.acknowledge();
        waker.wake_by_ref();
        assert_eq!(inbox.try_recv(), Ok(1));
    }

    /// A late worker cannot retain or revive the frontend after shutdown.
    #[test]
    fn a_worker_does_not_retain_the_frontend_sender() {
        let (sender, inbox) = channel();
        let sender = Arc::new(sender);
        let waker = Waker::from(Arc::new(InboxWake::new(&sender, || 1)));
        drop(sender);
        assert_eq!(inbox.try_recv(), Err(TryRecvError::Disconnected));
        waker.wake_by_ref();
        assert_eq!(inbox.try_recv(), Err(TryRecvError::Disconnected));
    }
}
