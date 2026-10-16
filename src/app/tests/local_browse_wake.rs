//! Local completion notifications keep their existing response queue authoritative.

use super::*;
use std::task::{Wake, Waker};

/// A hidden Local listing must be cached without replacing another screen's details.
#[cfg(feature = "local-browser")]
#[test]
fn local_completion_preserves_active_search_after_success() {
    assert_late_local_response_preserves_active_screen(true);
}

/// A failed hidden Local request must not replace another screen with a Local popup.
#[cfg(feature = "local-browser")]
#[test]
fn local_completion_preserves_active_search_after_failure() {
    assert_late_local_response_preserves_active_screen(false);
}

/// Holds the Local response until synthetic search results own the shared view again.
#[cfg(feature = "local-browser")]
fn assert_late_local_response_preserves_active_screen(success: bool) {
    let fixture = crate::test_support::canonical_tempdir("held Local response");
    let (mut app, _) = controller_with_mock_statuses([]);
    app.shutdown_local_browse_worker();
    let (requests, captured) = unbounded();
    let (responses, received) = unbounded();
    app.local_browse_requests = Some(requests);
    app.local_browse_responses = received;
    app.config.ui.show_local_folder_sizes = false;
    app.view.local_folder_sizes_enabled = false;
    app.view.local_path = fixture.path().display().to_string();
    app.show_screen(Screen::Local);
    let LocalBrowseRequest::Browse {
        generation,
        directory,
        ..
    } = captured.try_recv().expect("held Local request")
    else {
        panic!("expected a Local browse request");
    };
    assert_eq!(directory, fixture.path());
    assert!(app.view.local_browse_pending);
    app.show_screen(Screen::Search);
    app.view.search_query = "fixture query".to_owned();
    app.view.rows = ["First search result", "Second search result"]
        .into_iter()
        .map(|title| RowView {
            title: title.to_owned(),
            ..RowView::default()
        })
        .collect();
    app.view.details = Some(DetailView {
        title: "Second search result".to_owned(),
        description: "Fixture search details".to_owned(),
        ..DetailView::default()
    });
    app.view.status_line = "Fixture search status".to_owned();
    app.view.selected = 1;
    let rows = app.view.rows.clone();
    let details = app.view.details.clone();
    let status = app.view.status_line.clone();
    let selected = app.view.selected;
    let listing = crate::local_browser::LocalDirectoryListing {
        path: directory.clone(),
        parent: None,
        entries: vec![crate::local_browser::LocalEntry {
            name: "held-local-directory".into(),
            path: directory.join("held-local-directory"),
            kind: crate::local_browser::LocalEntryKind::Directory,
            size_bytes: None,
            image_dimensions: None,
            directory_identity: None,
        }],
        truncated: false,
        inspected_entries: 1,
    };
    responses
        .send(LocalBrowseResponse::Browse {
            generation,
            result: if success {
                Ok(listing.clone())
            } else {
                Err("Fixture Local failure".to_owned())
            },
        })
        .expect("deliver held Local response");
    app.drain_local_browse_responses(true);
    assert_eq!(app.view.screen, Screen::Search);
    assert_eq!(
        app.view.rows, rows,
        "a hidden Local response must not replace active rows"
    );
    assert_eq!(
        app.view.details, details,
        "a hidden Local response must not replace active details"
    );
    assert_eq!(app.view.status_line, status);
    assert_eq!(app.view.selected, selected);
    assert!(app.view.error_popup.is_none());
    assert!(!app.view.local_browse_pending);
    assert!(captured.is_empty());
    if success {
        assert_eq!(app.local_listing.as_ref(), Some(&listing));
        app.show_screen(Screen::Local);
        assert!(
            app.view
                .rows
                .iter()
                .any(|row| row.title == "held-local-directory")
        );
        assert!(
            captured.is_empty(),
            "returning to a cached Local listing must not browse again"
        );
    } else {
        assert!(app.local_listing.is_none());
    }
}

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
) -> (
    WorkerNotifier,
    Receiver<Result<LocalBrowseResponse, TryRecvError>>,
) {
    let (notifications, observed) = unbounded();
    let waker = Waker::from(Arc::new(ResponseProbe {
        responses,
        notifications,
    }));
    let notifier = WorkerNotifier::default();
    notifier.set_waker(Some(waker));
    assert!(matches!(observed.recv().unwrap(), Err(TryRecvError::Empty)));
    (notifier, observed)
}

#[test]
fn local_browse_wakes_after_success_error_and_disconnect_are_observable() {
    let fixture = crate::test_support::canonical_tempdir("Local completion fixture");
    let (requests, request_receiver) = unbounded();
    let (responses, response_receiver) = unbounded();
    let (waker, notifications) = response_probe(response_receiver);
    let worker = thread::spawn(move || {
        local_browse_worker(request_receiver, responses, waker);
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
        local_browse_worker(request_receiver, responses, waker);
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
    // Isolate this registration from unrelated provider workers started by the harness.
    controller.worker_notifier = WorkerNotifier::default();
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
    controller.set_worker_waker(Some(Waker::from(Arc::clone(&count))));
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
    controller.worker_notifier = WorkerNotifier::default();
    let (sender, responses) = unbounded();
    let publisher = LocalBrowseResponseSender::new(sender, controller.worker_notifier.clone());
    let count = Arc::new(WakeCount::default());
    controller.set_worker_waker(Some(Waker::from(Arc::clone(&count))));
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
    controller.set_worker_waker(None);
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
        let _publisher = LocalBrowseResponseSender::new(sender, waker);
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
    let publisher = LocalBrowseResponseSender::new(sender, waker);
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
