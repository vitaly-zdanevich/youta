//! Foreground copy regressions exercise the real filesystem worker and modal controller boundary.

use super::*;

/// A real source tree with manually driven controller channels, isolated from user state.
struct CopyFixture {
    controller: AppController,
    requests: Receiver<LocalBrowseRequest>,
    responses: Sender<LocalBrowseResponse>,
    source: PathBuf,
    destination: PathBuf,
    _directory: tempfile::TempDir,
}

impl CopyFixture {
    /// Builds file, nested-folder, and empty-folder sources without external helpers.
    fn new() -> Self {
        let directory = crate::test_support::canonical_tempdir("Local copy controller fixture");
        let source = directory.path().join("source 音楽");
        let destination = directory.path().join("destination");
        std::fs::create_dir_all(source.join("album/disc")).expect("source tree");
        std::fs::create_dir(source.join("album/empty")).expect("empty folder");
        std::fs::create_dir(&destination).expect("destination folder");
        std::fs::write(source.join("1.flac"), b"root audio").expect("source file");
        std::fs::write(source.join("album/disc/2.flac"), b"nested audio").expect("nested file");
        let mut config = Config::for_dir(directory.path().join("config"));
        config.ui.show_local_folder_sizes = false;
        let store = StateStore::open_in_memory().expect("in-memory state");
        let mut controller = AppController::new(config, store, None, None);
        controller.shutdown_local_browse_worker();
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(
            crate::local_browser::list_local_directory(
                &source,
                crate::local_browser::LocalBrowseLimits::default(),
            )
            .expect("source listing"),
        );
        controller.refresh_local_browser_rows();
        controller.select_local_path(Some(&source.join("1.flac")));
        let (sender, requests) = unbounded();
        let (responses, receiver) = unbounded();
        controller.local_browse_requests = Some(sender);
        controller.local_browse_responses = receiver;
        controller.local_browse_disconnect_reported = false;
        Self {
            controller,
            requests,
            responses,
            source,
            destination,
            _directory: directory,
        }
    }

    /// Applies the bounded listing requested by the current chooser, using exact paths.
    fn answer_destination_request(&mut self) {
        let LocalBrowseRequest::MoveDestinations {
            generation,
            directory,
        } = self.requests.try_recv().expect("destination request")
        else {
            panic!("expected a destination listing request");
        };
        let result = crate::local_move::list_local_move_destinations(
            &directory,
            LocalMoveDestinationLimits::default(),
        )
        .map_err(|error| error.to_string());
        self.controller
            .handle_local_browse_response(LocalBrowseResponse::MoveDestinations {
                generation,
                result,
            });
    }

    /// Opens Copy or Move and navigates into the real destination through shared chooser actions.
    fn choose_destination(&mut self, move_sources: bool) {
        self.controller.dispatch(if move_sources {
            UiAction::BeginLocalMove
        } else {
            UiAction::BeginLocalCopy
        });
        self.answer_destination_request();
        let index = self
            .controller
            .local_move_selection
            .as_ref()
            .expect("selection")
            .destination_rows
            .iter()
            .position(|path| path == &self.destination)
            .expect("destination row");
        self.controller
            .dispatch(UiAction::SelectLocalMoveDestination(index));
        self.controller
            .dispatch(UiAction::ActivateLocalMoveDestination);
        self.answer_destination_request();
        assert_eq!(
            self.controller
                .local_move_selection
                .as_ref()
                .unwrap()
                .destination_directory,
            self.destination
        );
    }

    /// Runs one accepted Copy on the production worker and delivers every progress/completion event.
    fn finish_copy_on_worker(&mut self) -> Vec<crate::local_move::LocalTransferProgress> {
        let request = self.requests.try_recv().expect("accepted copy request");
        assert!(matches!(&request, LocalBrowseRequest::Copy { .. }));
        let (sender, receiver) = unbounded();
        let (responses, results) = unbounded();
        let worker = thread::spawn(move || local_browse_worker(receiver, responses));
        sender.send(request).expect("send copy to worker");
        sender
            .send(LocalBrowseRequest::Shutdown)
            .expect("stop after copy");
        let mut progress_events = Vec::new();
        loop {
            let response = results
                .recv_timeout(Duration::from_secs(5))
                .expect("copy worker response");
            if let LocalBrowseResponse::TransferProgress { progress, .. } = &response {
                progress_events.push(*progress);
            }
            let complete = matches!(&response, LocalBrowseResponse::Copy { .. });
            self.controller.handle_local_browse_response(response);
            if complete {
                break;
            }
        }
        worker.join().expect("copy worker stopped");
        progress_events
    }
}

/// One representative worker update for generation and execution-lock tests.
fn progress_response(generation: u64) -> LocalBrowseResponse {
    LocalBrowseResponse::TransferProgress {
        generation,
        progress: crate::local_move::LocalTransferProgress {
            completed_bytes: 4,
            total_bytes: Some(10),
            completed_entries: 0,
            total_entries: 1,
        },
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end copy fixture verifies filesystem publication and unchanged source-owned state"
)]
fn local_copy_worker_preserves_sources_notes_queue_and_selection() {
    let mut fixture = CopyFixture::new();
    let source_file = fixture.source.join("1.flac");
    let source_folder = fixture.source.join("album");
    let file_target = CommentTarget::Media {
        media_id: local_media_id(&source_file),
    };
    let folder_target = CommentTarget::Source {
        source_id: local_media_id(&source_folder),
    };
    let file_note = fixture
        .controller
        .store
        .upsert_private_note(&file_target, "File note", 1)
        .unwrap();
    let folder_note = fixture
        .controller
        .store
        .upsert_private_note(&folder_target, "Folder note", 2)
        .unwrap();
    let queued =
        queue_item_from_local(&local_media_item_stub(source_file.clone(), Some(10))).unwrap();
    fixture.controller.current_media = Some(queued.media.id.clone());
    fixture.controller.playback_queue.push(queued);
    fixture.controller.current_autoplay_origin = Some(AutoplayOrigin::LocalBrowser {
        directory: fixture.source.clone(),
        entries: Arc::from([source_file.clone()]),
        index: 0,
    });
    fixture.controller.local_move_marks =
        HashSet::from([source_file.clone(), source_folder.clone()]);
    let queue = fixture.controller.playback_queue.clone();
    let origin = fixture.controller.current_autoplay_origin.clone();
    let current = fixture.controller.current_media.clone();
    fixture.choose_destination(false);
    fixture.controller.dispatch(UiAction::ConfirmLocalCopyHere);
    assert!(fixture.controller.local_move_is_executing());
    assert!(fixture.controller.view.local_file_progress.is_some());

    let progress = fixture.finish_copy_on_worker();

    let final_progress = progress.last().expect("worker publishes progress");
    assert_eq!(final_progress.completed_entries, 2);
    assert_eq!(final_progress.total_entries, 2);
    assert_eq!(final_progress.total_bytes, Some(22));
    assert_eq!(final_progress.completed_bytes, 22);
    assert!(!fixture.controller.local_move_is_executing());
    assert!(fixture.controller.view.local_file_progress.is_none());
    assert!(fixture.controller.view.local_file_popup.is_none());
    for root in [&fixture.source, &fixture.destination] {
        assert_eq!(std::fs::read(root.join("1.flac")).unwrap(), b"root audio");
        assert_eq!(
            std::fs::read(root.join("album/disc/2.flac")).unwrap(),
            b"nested audio"
        );
        assert!(root.join("album/empty").is_dir());
    }
    assert_eq!(fixture.controller.selected_local_path(), Some(source_file));
    assert_eq!(fixture.controller.playback_queue, queue);
    assert_eq!(fixture.controller.current_autoplay_origin, origin);
    assert_eq!(fixture.controller.current_media, current);
    assert_eq!(
        fixture.controller.store.private_note(&file_target).unwrap(),
        Some(file_note)
    );
    assert_eq!(
        fixture
            .controller
            .store
            .private_note(&folder_target)
            .unwrap(),
        Some(folder_note)
    );
    for target in [
        CommentTarget::Media {
            media_id: local_media_id(&fixture.destination.join("1.flac")),
        },
        CommentTarget::Source {
            source_id: local_media_id(&fixture.destination.join("album")),
        },
    ] {
        assert!(
            fixture
                .controller
                .store
                .private_note(&target)
                .unwrap()
                .is_none()
        );
    }
    #[cfg(any(feature = "local-move", feature = "local-rename"))]
    {
        assert!(fixture.controller.local_move_persistence_queue.is_empty());
        assert!(
            fixture
                .controller
                .store
                .local_move_intents()
                .unwrap()
                .is_empty()
        );
    }
}

/// The reducer rejects even direct IPC actions while a transfer owns its modal state.
fn assert_transfer_execution_locks_actions(move_sources: bool) {
    let mut fixture = CopyFixture::new();
    fixture.choose_destination(move_sources);
    fixture.controller.dispatch(if move_sources {
        UiAction::ConfirmLocalMoveHere
    } else {
        UiAction::ConfirmLocalCopyHere
    });
    assert!(fixture.controller.local_move_is_executing());
    let _accepted = fixture.requests.try_recv().expect("one accepted transfer");
    let selected = fixture.controller.view.selected;
    let rows = fixture.controller.view.rows.clone();
    let popup = fixture.controller.view.local_file_popup.clone();
    let queue = fixture.controller.playback_queue.clone();
    let generation = fixture.controller.local_move_generation;
    fixture.controller.session_dirty = false;
    for action in [
        UiAction::SelectRow(0),
        UiAction::MoveSelection(-1),
        UiAction::ActivateSelection,
        UiAction::ShowScreen(Screen::Search),
        UiAction::BeginSearch,
        UiAction::GoBack,
        UiAction::OpenDroppedPaths(vec![fixture.destination.clone()]),
        UiAction::OpenPreferences,
        UiAction::BeginLocalCopy,
        UiAction::BeginLocalMove,
        UiAction::BeginLocalRename,
        UiAction::RequestLocalTrash,
        UiAction::ConfirmLocalTrash,
        UiAction::ConfirmLocalCopyHere,
        UiAction::ConfirmLocalMoveHere,
        UiAction::SelectLocalMoveDestination(0),
        UiAction::MoveLocalMoveDestination(1),
        UiAction::ActivateLocalMoveDestination,
        UiAction::DismissLocalFilePopup,
        UiAction::EditPrivateNote,
        UiAction::AddToQueue,
        UiAction::PlayQueueNeighbour(1),
        UiAction::TogglePause,
        UiAction::Quit,
    ] {
        fixture.controller.dispatch(action);
        assert_eq!(fixture.controller.view.screen, Screen::Local);
        assert_eq!(fixture.controller.view.selected, selected);
        assert_eq!(fixture.controller.view.rows, rows);
        assert_eq!(fixture.controller.view.local_file_popup, popup);
        assert_eq!(fixture.controller.playback_queue, queue);
        assert_eq!(fixture.controller.local_move_generation, generation);
        assert!(!fixture.controller.view.quitting);
        assert!(!fixture.controller.session_dirty);
        assert!(fixture.requests.try_recv().is_err());
    }
    fixture
        .responses
        .send(progress_response(generation))
        .expect("progress queued");
    fixture.controller.tick();
    assert_eq!(
        fixture
            .controller
            .view
            .local_file_progress
            .as_ref()
            .unwrap()
            .completed_bytes,
        4
    );
    // No filesystem request was executed by this modal-only test.
    fixture.controller.local_move_execution_pending = false;
    fixture.controller.view.local_file_progress = None;
}

#[test]
fn local_copy_execution_blocks_queued_actions_but_tick_accepts_progress() {
    assert_transfer_execution_locks_actions(false);
}

#[cfg(feature = "local-move")]
#[test]
fn local_move_execution_blocks_queued_actions_but_tick_accepts_progress() {
    assert_transfer_execution_locks_actions(true);
}

#[test]
fn local_copy_listing_can_be_cancelled_and_late_listing_cannot_reopen_it() {
    let mut fixture = CopyFixture::new();
    fixture.controller.dispatch(UiAction::BeginLocalCopy);
    assert!(!fixture.controller.local_move_is_executing());
    assert!(fixture.controller.view.local_file_progress.is_none());
    fixture.controller.dispatch(UiAction::DismissLocalFilePopup);
    fixture.answer_destination_request();
    assert!(fixture.controller.view.local_file_popup.is_none());
    assert!(fixture.controller.local_move_selection.is_none());
    assert!(fixture.source.join("1.flac").exists());
}

#[test]
fn local_copy_stale_progress_and_completions_do_not_change_operation_ownership() {
    let mut fixture = CopyFixture::new();
    fixture.choose_destination(false);
    fixture.controller.dispatch(UiAction::ConfirmLocalCopyHere);
    let _accepted = fixture.requests.try_recv().expect("copy request");
    let generation = fixture.controller.local_move_generation;
    let initial = fixture.controller.view.local_file_progress.clone();
    fixture
        .controller
        .handle_local_browse_response(progress_response(generation.wrapping_sub(1)));
    fixture
        .controller
        .handle_local_browse_response(LocalBrowseResponse::Copy {
            generation: generation.wrapping_sub(1),
            result: Ok(crate::local_move::LocalCopyReport::default()),
        });
    #[cfg(feature = "local-move")]
    fixture
        .controller
        .handle_local_browse_response(LocalBrowseResponse::Move {
            generation,
            planned: Vec::new(),
            result: Err(crate::local_move::LocalMoveError::Validation(
                crate::local_move::LocalMoveValidationError::EmptyBatch,
            )),
        });
    assert!(fixture.controller.local_move_is_executing());
    assert_eq!(fixture.controller.view.local_file_progress, initial);
    fixture
        .controller
        .handle_local_browse_response(progress_response(generation));
    assert_eq!(
        fixture
            .controller
            .view
            .local_file_progress
            .as_ref()
            .unwrap()
            .completed_bytes,
        4
    );
    fixture
        .controller
        .handle_local_browse_response(LocalBrowseResponse::Copy {
            generation,
            result: Ok(crate::local_move::LocalCopyReport::default()),
        });
    assert!(!fixture.controller.local_move_is_executing());
    fixture
        .controller
        .handle_local_browse_response(progress_response(generation));
    assert!(fixture.controller.view.local_file_progress.is_none());
    assert!(fixture.controller.view.local_file_popup.is_none());
    fixture.controller.dispatch(UiAction::BeginSearch);
    assert!(fixture.controller.view.search_editing);
}

#[test]
fn local_copy_collision_unlocks_and_preserves_original_and_destination_bytes() {
    let mut fixture = CopyFixture::new();
    std::fs::write(fixture.destination.join("1.flac"), b"existing destination").unwrap();
    fixture.choose_destination(false);
    fixture.controller.dispatch(UiAction::ConfirmLocalCopyHere);
    fixture.finish_copy_on_worker();
    assert!(!fixture.controller.local_move_is_executing());
    assert!(fixture.controller.view.local_file_progress.is_none());
    assert!(matches!(
        &fixture.controller.view.local_file_popup,
        Some(LocalFilePopupView::Copy {
            pending: false,
            error: Some(_),
            ..
        })
    ));
    assert_eq!(
        std::fs::read(fixture.source.join("1.flac")).unwrap(),
        b"root audio"
    );
    assert_eq!(
        std::fs::read(fixture.destination.join("1.flac")).unwrap(),
        b"existing destination"
    );
    assert_eq!(
        fixture
            .controller
            .local_move_selection
            .as_ref()
            .unwrap()
            .sources,
        [fixture.source.join("1.flac")]
    );
}

#[test]
fn local_copy_enqueue_failure_unlocks_the_dialog_without_touching_files() {
    let mut fixture = CopyFixture::new();
    fixture.choose_destination(false);
    fixture.controller.local_browse_requests = None;
    fixture.controller.dispatch(UiAction::ConfirmLocalCopyHere);
    assert!(!fixture.controller.local_move_is_executing());
    assert!(fixture.controller.view.local_file_progress.is_none());
    assert!(matches!(
        &fixture.controller.view.local_file_popup,
        Some(LocalFilePopupView::Copy {
            pending: false,
            error: Some(_),
            ..
        })
    ));
    assert!(fixture.source.join("1.flac").exists());
    assert!(!fixture.destination.join("1.flac").exists());
    assert!(fixture.requests.try_recv().is_err());
}

#[test]
fn local_copy_worker_disconnect_unlocks_without_creating_move_recovery_intent() {
    let mut fixture = CopyFixture::new();
    fixture.choose_destination(false);
    fixture.controller.dispatch(UiAction::ConfirmLocalCopyHere);
    let _accepted = fixture.requests.try_recv().expect("copy request");
    let (sender, receiver) = unbounded();
    drop(sender);
    fixture.controller.local_browse_responses = receiver;
    fixture.controller.drain_local_browse_responses(false);
    assert!(!fixture.controller.local_move_is_executing());
    assert!(fixture.controller.view.local_file_progress.is_none());
    assert!(matches!(&fixture.controller.view.local_file_popup,
        Some(LocalFilePopupView::Copy { pending: false, error: Some(message), .. })
            if message.contains("Originals are unchanged")));
    assert!(fixture.source.join("1.flac").exists());
    #[cfg(any(feature = "local-move", feature = "local-rename"))]
    {
        assert!(!fixture.controller.local_move_journal_pending);
        assert!(
            fixture
                .controller
                .store
                .local_move_intents()
                .unwrap()
                .is_empty()
        );
    }
}

/// Fatal-mode shutdown must resolve the transfer lock before showing its error-only UI.
#[test]
fn local_copy_fatal_diagnostic_unlocks_progress_and_allows_error_dismissal() {
    let mut fixture = CopyFixture::new();
    fixture.choose_destination(false);
    fixture.controller.dispatch(UiAction::ConfirmLocalCopyHere);
    let _accepted = fixture.requests.try_recv().expect("accepted copy request");
    assert!(fixture.controller.local_move_is_executing());
    fixture
        .controller
        .enter_fatal_diagnostic_mode("Fatal copy fixture", "fatal report");

    assert!(fixture.controller.diagnostic_only);
    assert!(!fixture.controller.local_move_is_executing());
    assert!(fixture.controller.view.local_file_progress.is_none());
    assert!(fixture.controller.view.error_popup.is_some());
    assert!(!fixture.controller.view.quitting);
    fixture.controller.dispatch(UiAction::DismissErrorPopup);
    assert!(fixture.controller.view.error_popup.is_none());
    assert!(fixture.controller.view.quitting);
    assert_eq!(
        std::fs::read(fixture.source.join("1.flac")).unwrap(),
        b"root audio"
    );
}
