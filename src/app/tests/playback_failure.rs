//! Concise network-failure guidance without losing retries or copyable diagnostics.

use super::*;

/// mpv may retain the timeout in any one field, with different capitalization.
#[test]
fn playback_timeout_detection_checks_all_backend_fields() {
    for field in 0..3 {
        for message in ["Read timed out", "CONNECTION TIMED OUT", "TimeoutError"] {
            let mut end = PlaybackEnd {
                reason: PlaybackEndReason::Error,
                error: None,
                file_error: None,
                diagnostic: None,
            };
            match field {
                0 => end.error = Some(message.to_owned()),
                1 => end.file_error = Some(message.to_owned()),
                _ => end.diagnostic = Some(message.to_owned()),
            }
            assert!(playback_end_reports_timeout(&end));
        }
    }
}

/// Reproduces a network timeout hidden behind mpv's generic demuxer failure.
fn timed_out_end(reason: PlaybackEndReason) -> PlaybackEnd {
    PlaybackEnd {
        reason,
        error: Some("loading failed".to_owned()),
        file_error: Some("unrecognized file format".to_owned()),
        diagnostic: Some("[ytdl_hook] Read timed out".to_owned()),
    }
}

/// A terminal timeout stays concise while preserving the checked-format retry and raw report.
#[test]
fn playback_timeout_keeps_retries_and_copyable_diagnostics_without_system_info_in_summary() {
    for (started, reason) in [
        (false, PlaybackEndReason::Error),
        (false, PlaybackEndReason::Eof),
        (true, PlaybackEndReason::Error),
    ] {
        let failure = timed_out_end(reason);
        let (mut controller, state, _, events) = controller_with_mock_lifecycle([], []);
        controller.config.providers.mpv_executable = "/nonexistent/youta-test-mpv".into();
        controller.config.providers.yt_dlp_executable = "/nonexistent/youta-test-yt-dlp".into();
        let calls = Arc::new(Mutex::new(Vec::new()));
        controller.report_actions = Box::new(MockDiagnosticActions {
            calls: Arc::clone(&calls),
            gh_available: true,
            submission_result: Mutex::new(None),
        });
        controller.play_queue_item(fixture_youtube_item("first"), false);
        controller
            .playback_queue
            .push(fixture_direct_item("second"));
        {
            let mut events = events.lock().unwrap();
            events.push_back(PlaybackEvent::MediaLoaded);
            if started {
                events.push_back(PlaybackEvent::PlaybackStarted);
            }
            events.push_back(PlaybackEvent::Ended(failure.clone()));
        }
        controller.update_player();
        if !started {
            assert!(controller.view.error_popup.is_none());
            assert_eq!(
                controller.playback_load_kind,
                PlaybackLoadKind::YouTubeChecked
            );
            events
                .lock()
                .unwrap()
                .push_back(PlaybackEvent::Ended(failure));
            controller.update_player();
        }
        let popup = controller.view.error_popup.as_ref().expect("timeout popup");
        assert_eq!(
            popup.summary.as_deref(),
            Some("Playback timed out. Please try again.")
        );
        assert!(popup.report.contains("unrecognized file format"));
        assert!(popup.report.contains("Read timed out"));
        assert!(!popup.reportable);
        assert!(!popup.gh_available);
        let report = popup.report.clone();
        assert!(
            controller.diagnostic_helpers_cache.is_none(),
            "a routine timeout must not probe helper versions"
        );
        assert!(!controller.view.status_line.contains('\n'));
        assert!(controller.view.status_line.contains("Playback timed out"));
        assert_eq!(controller.playback_phase, PlaybackPhase::Idle);
        assert!(!controller.view.playback_starting);
        assert!(controller.current_media.is_none());
        assert_eq!(controller.playback_queue.current_index, Some(0));
        assert_eq!(
            state.lock().unwrap().played.len(),
            if started { 1 } else { 2 }
        );
        if !started {
            assert!(controller.store.history(false, 10).unwrap().is_empty());
        }
        controller.dispatch(UiAction::CopyErrorReport);
        controller.dispatch(UiAction::RequestGitHubIssueSubmission);
        controller.dispatch(UiAction::ConfirmGitHubIssueSubmission);
        assert_eq!(*calls.lock().unwrap(), [DiagnosticCall::Copy(report)]);
    }
}

/// Deferring a timeout behind an existing issue submission retains its concise body.
#[test]
fn playback_timeout_summary_survives_diagnostic_deferral() {
    let (mut controller, _, _, _) = controller_with_mock_lifecycle([], []);
    controller.diagnostic_helpers_cache = Some(Vec::new());
    controller.show_diagnostic_report("Earlier error", "earlier report");
    controller.pinned_github_issue_submission_generation = Some(42);
    controller.handle_playback_end(timed_out_end(PlaybackEndReason::Error), Duration::ZERO);
    assert_eq!(
        controller.view.error_popup.as_ref().unwrap().report,
        "earlier report"
    );
    assert_eq!(controller.deferred_diagnostic_reports.len(), 1);
    controller.pinned_github_issue_submission_generation = None;
    assert!(controller.show_deferred_diagnostic_report());
    let popup = controller.view.error_popup.as_ref().unwrap();
    assert_eq!(
        popup.summary.as_deref(),
        Some("Playback timed out. Please try again.")
    );
    assert!(popup.report.contains("Read timed out"));
    assert!(!popup.reportable);
}

/// Decoder and process faults still expose their diagnostics rather than timeout advice.
#[test]
fn unrelated_playback_failures_keep_the_diagnostic_presentation() {
    for diagnostic in [
        "Could not initialize audio output",
        "Unsupported audio codec",
    ] {
        let (mut controller, _, _, _) = controller_with_mock_lifecycle([], []);
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.handle_playback_end(
            PlaybackEnd {
                reason: PlaybackEndReason::Error,
                error: Some("loading failed".to_owned()),
                file_error: Some("unrecognized file format".to_owned()),
                diagnostic: Some(diagnostic.to_owned()),
            },
            Duration::ZERO,
        );
        let popup = controller.view.error_popup.as_ref().unwrap();
        assert!(popup.summary.is_none());
        assert!(popup.reportable);
        assert!(popup.report.contains(diagnostic));
    }
}

/// A timeout in ancillary output cannot hide the existing forbidden/decoder guidance.
#[test]
fn playback_timeout_keeps_http_403_and_tracker_setup_guidance() {
    for tracker_module in [false, true] {
        let (mut controller, _, _, _) = controller_with_mock_lifecycle([], []);
        controller.diagnostic_helpers_cache = Some(Vec::new());
        #[cfg(feature = "yt-dlp")]
        let _requests = capture_controller_provider_requests(&mut controller);
        let mut end = timed_out_end(PlaybackEndReason::Error);
        let message = if tracker_module {
            tracker_decoder_help(&playback_end_message(&end))
        } else {
            end.file_error = Some("HTTP Error 403: Forbidden".to_owned());
            playback_end_message(&end)
        };
        controller.show_playback_end_error("Playback failed", message, &end, tracker_module);
        let popup = controller.view.error_popup.as_ref().unwrap();
        assert!(popup.summary.is_none());
        assert!(
            popup
                .report
                .contains(if tracker_module { "openmpt" } else { "403" })
        );
        #[cfg(feature = "yt-dlp")]
        assert_eq!(popup.yt_dlp_forbidden.is_some(), !tracker_module);
    }
}
