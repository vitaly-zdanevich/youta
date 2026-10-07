//! Local completion notifications keep their existing response queue authoritative.

use super::*;
use std::task::{Wake, Waker};

/// Drains immediately when woken, exposing a notification sent before its data.
struct ResponseProbe {
    responses: Receiver<LocalBrowseResponse>,
    notifications: Sender<Result<LocalBrowseResponse, TryRecvError>>,
}

impl Wake for ResponseProbe {
    fn wake(self: Arc<Self>) {
        let _ = self.notifications.send(self.responses.try_recv());
    }
}

/// Counts registration and completion signals without consuming queued results.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Builds an immediate response observer without a timer or frontend thread.
fn response_probe(
    responses: Receiver<LocalBrowseResponse>,
) -> (Waker, Receiver<Result<LocalBrowseResponse, TryRecvError>>) {
    let (notifications, observed) = unbounded();
    let waker = Waker::from(Arc::new(ResponseProbe {
        responses,
        notifications,
    }));
    (waker, observed)
}

#[test]
fn local_browse_wakes_after_success_error_and_disconnect_are_observable() {
    let fixture = crate::test_support::canonical_tempdir("Local completion fixture");
    let (requests, request_receiver) = unbounded();
    let (responses, response_receiver) = unbounded();
    let (waker, notifications) = response_probe(response_receiver);
    let worker = thread::spawn(move || {
        local_browse_worker(
            request_receiver,
            responses,
            Arc::new(Mutex::new(Some(waker))),
        );
    });
    for (generation, directory, succeeds) in [
        (1, fixture.path().to_owned(), true),
        (2, fixture.path().join("missing"), false),
    ] {
        requests
            .send(LocalBrowseRequest::Browse {
                generation,
                directory,
                preferred_child: None,
                options: crate::local_browser::LocalBrowseOptions::default(),
            })
            .expect("request listing");
        let observed = notifications
            .recv_timeout(Duration::from_secs(2))
            .expect("completion wake");
        let Ok(LocalBrowseResponse::Browse {
            generation: actual,
            result,
        }) = observed
        else {
            panic!("wake must observe the queued listing");
        };
        assert_eq!(actual, generation);
        assert_eq!(result.is_ok(), succeeds);
    }
    requests
        .send(LocalBrowseRequest::Shutdown)
        .expect("stop worker");
    worker.join().expect("worker joins");
    assert!(matches!(
        notifications
            .recv_timeout(Duration::from_secs(2))
            .expect("disconnect wake"),
        Err(TryRecvError::Disconnected)
    ));
}

#[cfg(any(feature = "local-move", feature = "local-copy"))]
#[test]
fn local_copy_and_move_destination_completion_wakes_after_success_and_error() {
    let fixture = crate::test_support::canonical_tempdir("Local destination completion fixture");
    let (requests, request_receiver) = unbounded();
    let (responses, response_receiver) = unbounded();
    let (waker, notifications) = response_probe(response_receiver);
    let worker = thread::spawn(move || {
        local_browse_worker(
            request_receiver,
            responses,
            Arc::new(Mutex::new(Some(waker))),
        );
    });
    // Both choosers intentionally share this directory-only request variant.
    for (generation, directory, succeeds) in [
        (3, fixture.path().to_owned(), true),
        (4, fixture.path().join("missing"), false),
    ] {
        requests
            .send(LocalBrowseRequest::MoveDestinations {
                generation,
                directory,
            })
            .expect("request destinations");
        let observed = notifications
            .recv_timeout(Duration::from_secs(2))
            .expect("completion wake");
        let Ok(LocalBrowseResponse::MoveDestinations {
            generation: actual,
            result,
        }) = observed
        else {
            panic!("wake must observe the queued destinations");
        };
        assert_eq!(actual, generation);
        assert_eq!(result.is_ok(), succeeds);
    }
    requests
        .send(LocalBrowseRequest::Shutdown)
        .expect("stop worker");
    worker.join().expect("worker joins");
}

#[test]
fn local_browse_registration_wakes_for_results_completed_before_registration() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.shutdown_local_browse_worker();
    let (responses, response_receiver) = unbounded();
    controller.local_browse_responses = response_receiver;
    assert!(
        responses
            .send(LocalBrowseResponse::Browse {
                generation: 9,
                result: Err("completed before frontend attachment".to_owned()),
            })
            .is_ok()
    );
    drop(responses);
    let count = Arc::new(WakeCount::default());
    controller.set_local_browse_waker(Some(Waker::from(Arc::clone(&count))));
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(
        matches!(
            controller.local_browse_responses.try_recv(),
            Ok(LocalBrowseResponse::Browse { generation: 9, .. })
        ),
        "registration must leave the queued response for the controller"
    );
    assert!(matches!(
        controller.local_browse_responses.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
}

#[test]
fn local_browse_detachment_stops_notifications_without_dropping_results() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    controller.shutdown_local_browse_worker();
    let (sender, responses) = unbounded();
    let publisher = LocalBrowseResponseSender {
        sender: Some(sender),
        waker: Arc::clone(&controller.local_browse_waker),
    };
    let count = Arc::new(WakeCount::default());
    controller.set_local_browse_waker(Some(Waker::from(Arc::clone(&count))));
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(
        publisher
            .send(LocalBrowseResponse::Browse {
                generation: 10,
                result: Err("first".to_owned())
            })
            .is_ok()
    );
    assert_eq!(count.0.load(Ordering::SeqCst), 2);
    controller.set_local_browse_waker(None);
    assert!(
        publisher
            .send(LocalBrowseResponse::Browse {
                generation: 11,
                result: Err("second".to_owned())
            })
            .is_ok()
    );
    drop(publisher);
    assert_eq!(count.0.load(Ordering::SeqCst), 2);
    for generation in [10, 11] {
        assert!(
            matches!(responses.try_recv(), Ok(LocalBrowseResponse::Browse { generation: actual, .. }) if actual == generation)
        );
    }
    assert!(matches!(
        responses.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
}

#[test]
fn local_browse_unwind_wakes_only_after_worker_sender_disconnects() {
    let (sender, responses) = unbounded();
    let (waker, notifications) = response_probe(responses);
    let worker = thread::spawn(move || {
        let _publisher = LocalBrowseResponseSender {
            sender: Some(sender),
            waker: Arc::new(Mutex::new(Some(waker))),
        };
        panic!("simulated filesystem worker failure");
    });
    assert!(worker.join().is_err());
    assert!(matches!(
        notifications
            .recv_timeout(Duration::from_secs(2))
            .expect("unwind wake"),
        Err(TryRecvError::Disconnected)
    ));
}

/// A failed directory worker must not leave either destination chooser loading.
#[cfg(any(feature = "local-move", feature = "local-copy"))]
#[test]
fn local_destination_chooser_stops_loading_after_worker_disconnect() {
    for copy in [false, true] {
        let (mut controller, _) = controller_with_mock_statuses([]);
        controller.shutdown_local_browse_worker();
        controller.view.local_file_popup = Some(if copy {
            LocalFilePopupView::Copy {
                source_names: vec!["song.flac".to_owned()],
                destination: "/music".to_owned(),
                directories: Vec::new(),
                selected: 0,
                pending: true,
                error: None,
            }
        } else {
            LocalFilePopupView::Move {
                source_names: vec!["song.flac".to_owned()],
                destination: "/music".to_owned(),
                directories: Vec::new(),
                selected: 0,
                pending: true,
                error: None,
            }
        });
        assert!(!controller.local_move_execution_pending);
        controller.drain_local_browse_responses(false);
        let Some(
            LocalFilePopupView::Move { pending, error, .. }
            | LocalFilePopupView::Copy { pending, error, .. },
        ) = controller.view.local_file_popup.as_ref()
        else {
            panic!("destination chooser remains available after disconnection");
        };
        assert!(
            !pending,
            "copy={copy}: a disconnected listing cannot stay pending"
        );
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("listing destinations"))
        );
        assert!(!controller.local_move_execution_pending);
        assert!(controller.view.local_file_progress.is_none());
        #[cfg(feature = "local-move")]
        assert!(!controller.local_move_journal_pending);
    }
}

#[cfg(any(feature = "local-move", feature = "local-copy"))]
#[test]
fn local_transfer_progress_wakes_after_enqueueing_its_payload() {
    let (sender, responses) = unbounded();
    let (waker, notifications) = response_probe(responses);
    let publisher = LocalBrowseResponseSender {
        sender: Some(sender),
        waker: Arc::new(Mutex::new(Some(waker))),
    };
    let progress = crate::local_move::LocalTransferProgress {
        completed_bytes: 2,
        total_bytes: Some(3),
        completed_entries: 0,
        total_entries: 1,
    };
    local_transfer_progress_sender(&publisher, 12)(progress);
    assert!(matches!(
        notifications.try_recv(),
        Ok(Ok(LocalBrowseResponse::TransferProgress { generation: 12, progress: actual })) if actual == progress
    ));
}
