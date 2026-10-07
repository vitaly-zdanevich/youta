//! Response publication keeps its existing queue while waking the frontend.

use super::*;

/// Sends payloads before notification and closes each sender before its final wake.
pub(super) struct ResponseSender<T> {
    sender: Option<Sender<T>>,
    notifier: WorkerNotifier,
}

impl<T> ResponseSender<T> {
    /// Wraps the worker's existing bounded or unbounded channel without copying data.
    pub(super) fn new(sender: Sender<T>, notifier: WorkerNotifier) -> Self {
        Self {
            sender: Some(sender),
            notifier,
        }
    }

    /// Publishes a reply before signalling that the controller may drain it.
    pub(super) fn send(&self, value: T) -> Result<(), ()> {
        self.sender
            .as_ref()
            .expect("attached response sender")
            .send(value)
            .map_err(|_| ())?;
        self.notifier.wake();
        Ok(())
    }

    /// Preserves latest-only bounded-channel policies without waking for failed sends.
    #[cfg(any(feature = "yt-dlp", feature = "bandcamp", feature = "invidious", test))]
    pub(super) fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.sender
            .as_ref()
            .expect("attached response sender")
            .try_send(value)?;
        self.notifier.wake();
        Ok(())
    }
}

impl<T> Clone for ResponseSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            notifier: self.notifier.clone(),
        }
    }
}

impl<T> From<Sender<T>> for ResponseSender<T> {
    fn from(sender: Sender<T>) -> Self {
        Self::new(sender, WorkerNotifier::default())
    }
}

impl<T> Drop for ResponseSender<T> {
    fn drop(&mut self) {
        drop(self.sender.take());
        self.notifier.wake();
    }
}

/// Consumes a final reply without waiting for the sender thread to be scheduled again.
///
/// One-shot workers do no work after sending; dropping their unfinished handles
/// detaches only final thread cleanup, never an in-flight provider operation.
#[cfg(any(
    feature = "soundcloud",
    feature = "archive-org",
    feature = "invidious",
    feature = "web-browser",
    feature = "archive-upload",
    feature = "s3-upload"
))]
pub(super) fn reap_published_worker(thread: JoinHandle<()>) {
    if thread.is_finished() {
        let _ = thread.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Wake, Waker};

    struct Probe {
        responses: Receiver<u8>,
        observed: Sender<Result<u8, TryRecvError>>,
    }

    impl Wake for Probe {
        fn wake(self: Arc<Self>) {
            let _ = self.observed.send(self.responses.try_recv());
        }
    }

    /// A synchronous frontend observes data or disconnect before each notification.
    #[test]
    fn response_publication_and_disconnect_precede_wake() {
        let (sender, responses) = bounded(1);
        let (observed, observations) = unbounded();
        let notifier = WorkerNotifier::default();
        notifier.set_waker(Some(Waker::from(Arc::new(Probe {
            responses,
            observed,
        }))));
        assert_eq!(observations.recv().unwrap(), Err(TryRecvError::Empty));
        let sender = ResponseSender::new(sender, notifier);
        sender.send(1).unwrap();
        assert_eq!(observations.recv().unwrap(), Ok(1));
        sender.try_send(2).unwrap();
        assert_eq!(observations.recv().unwrap(), Ok(2));
        drop(sender);
        assert_eq!(
            observations.recv().unwrap(),
            Err(TryRecvError::Disconnected)
        );
    }

    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A full latest-only channel must not invent a completion or lose its payload.
    #[test]
    fn unsuccessful_try_send_does_not_wake() {
        let (sender, responses) = bounded(1);
        let notifier = WorkerNotifier::default();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        notifier.set_waker(Some(Waker::from(count.clone())));
        let sender = ResponseSender::new(sender, notifier);
        sender.try_send(1).unwrap();
        assert_eq!(count.0.load(Ordering::SeqCst), 2);
        assert!(matches!(sender.try_send(2), Err(TrySendError::Full(2))));
        assert_eq!(count.0.load(Ordering::SeqCst), 2);
        assert_eq!(responses.recv().unwrap(), 1);
    }
}
