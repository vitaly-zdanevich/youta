//! Manual-report regressions use a mocked submission/clipboard boundary only.

use super::*;
use crate::view::{BugReportField, BugReportPopupView};

/// Builds a report controller without launching GitHub or a clipboard helper.
fn bug_report_controller(gh_available: bool) -> (AppController, Arc<Mutex<Vec<DiagnosticCall>>>) {
    let (mut controller, _) = controller_with_mock_statuses(Vec::new());
    let calls = Arc::new(Mutex::new(Vec::new()));
    controller.report_actions = Box::new(MockDiagnosticActions {
        calls: Arc::clone(&calls),
        gh_available,
        submission_result: Mutex::new(None),
    });
    controller.view.youtube_setup_popup = None;
    (controller, calls)
}

/// Supplies user fields through the same shared action vocabulary as a frontend.
fn fill_bug_report(controller: &mut AppController) {
    for character in "Playback loses position".chars() {
        controller.dispatch(UiAction::AppendBugReportCharacter(character));
    }
    controller.dispatch(UiAction::SelectBugReportField(BugReportField::Body));
    for character in "Steps:\n1. Play a track\n2. Seek".chars() {
        controller.dispatch(UiAction::AppendBugReportCharacter(character));
    }
}

#[test]
fn bug_report_submits_directly_once_without_browser_or_error_title_prefix() {
    let (mut controller, calls) = bug_report_controller(true);
    controller.view.external_opener_available = false;
    controller.open_bug_report(Some("ASCII fixture".into()));
    assert!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .with_screenshot
    );
    fill_bug_report(&mut controller);
    assert!(calls.lock().unwrap().is_empty());
    controller.dispatch(UiAction::SubmitBugReport);
    let popup = controller.view.bug_report_popup.as_ref().unwrap();
    assert_eq!(popup.submission, GitHubIssueSubmissionView::Submitting);
    let footer = popup.footer.clone();
    controller.dispatch(UiAction::SubmitBugReport);
    controller.dispatch(UiAction::AppendBugReportCharacter('x'));
    controller.dispatch(UiAction::DismissBugReport);
    assert!(controller.view.bug_report_popup.is_some());
    let sent = calls.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let DiagnosticCall::Submit { title, report } = &sent[0] else {
        panic!("direct submission")
    };
    assert_eq!(title, "Playback loses position");
    assert!(report.contains("ASCII fixture"));
    assert!(report.ends_with(&footer));
    assert_eq!(report.matches("\n---\n").count(), 1);
    assert!(!report.contains("Cargo.lock"));
    controller.tick();
    assert!(matches!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .submission,
        GitHubIssueSubmissionView::Submitted { .. }
    ));
    controller.dispatch(UiAction::SubmitBugReport);
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn bug_report_screenshot_opt_out_and_sensitive_modal_preserve_drafts() {
    let (mut controller, calls) = bug_report_controller(false);
    controller.view.private_note_popup = Some(PrivateNotePopupView {
        body: "private diary".into(),
        ..PrivateNotePopupView::default()
    });
    assert!(!controller.view.bug_report_screenshot_allowed());
    controller.open_bug_report(Some("private diary".into()));
    let popup = controller.view.bug_report_popup.as_ref().unwrap();
    assert!(popup.with_screenshot);
    assert!(!popup.screenshot_available);
    assert!(popup.screenshot_notice.is_some());
    controller.dispatch(UiAction::ToggleBugReportScreenshot);
    let popup = controller.view.bug_report_popup.as_ref().unwrap();
    assert!(
        popup.with_screenshot,
        "the unavailable checkbox must stay disabled for direct actions"
    );
    assert_eq!(popup.selected_field, BugReportField::Title);
    fill_bug_report(&mut controller);
    controller.open_bug_report(Some("replacement capture".into()));
    assert_eq!(
        controller.view.bug_report_popup.as_ref().unwrap().title,
        "Playback loses position"
    );
    controller.dispatch(UiAction::CopyBugReport);
    assert!(
        matches!(&calls.lock().unwrap()[0], DiagnosticCall::Copy(body)
		if !body.contains("private diary") && !body.contains("replacement capture"))
    );
    controller.dispatch(UiAction::DismissBugReport);
    assert_eq!(
        controller.view.private_note_popup.as_ref().unwrap().body,
        "private diary"
    );
    controller.view.private_note_popup = None;
    controller.open_bug_report(Some("optional screenshot".into()));
    fill_bug_report(&mut controller);
    controller.dispatch(UiAction::ToggleBugReportScreenshot);
    assert_eq!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .selected_field,
        BugReportField::Screenshot,
        "clicking the checkbox must also give it keyboard focus"
    );
    controller.dispatch(UiAction::CopyBugReport);
    assert!(
        matches!(calls.lock().unwrap().last(), Some(DiagnosticCall::Copy(body))
		if !body.contains("optional screenshot"))
    );
}

#[test]
fn bug_report_missing_gh_keeps_copy_and_browser_free_guidance() {
    let (mut controller, calls) = bug_report_controller(false);
    controller.view.external_opener_available = false;
    controller.dispatch(UiAction::OpenBugReport);
    fill_bug_report(&mut controller);
    controller.dispatch(UiAction::SubmitBugReport);
    assert!(calls.lock().unwrap().is_empty());
    let popup = controller.view.bug_report_popup.as_ref().unwrap();
    assert!(popup.validation_error.as_deref().unwrap().contains("gh"));
    assert_eq!(popup.submission, GitHubIssueSubmissionView::Idle);
    controller.dispatch(UiAction::CopyBugReport);
    assert!(matches!(&calls.lock().unwrap()[0], DiagnosticCall::Copy(_)));
}

#[test]
fn bug_report_edits_graphemes_and_rejects_blank_or_overlong_fields() {
    let (mut controller, calls) = bug_report_controller(true);
    controller.dispatch(UiAction::OpenBugReport);
    controller.dispatch(UiAction::SubmitBugReport);
    assert!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .validation_error
            .is_some()
    );
    for character in "e\u{301}👩‍💻".chars() {
        controller.dispatch(UiAction::AppendBugReportCharacter(character));
    }
    controller.dispatch(UiAction::DeleteBugReportCharacter);
    assert_eq!(
        controller.view.bug_report_popup.as_ref().unwrap().title,
        "e\u{301}"
    );
    controller.dispatch(UiAction::MoveBugReportCursor(PrivateNoteCursorMotion::Home));
    controller.dispatch(UiAction::DeleteBugReportForward);
    assert!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .title
            .is_empty()
    );
    let popup = controller.view.bug_report_popup.as_mut().unwrap();
    popup.title = "x".repeat(crate::report_actions::MAX_ISSUE_TITLE_CHARS + 1);
    popup.body = "body".into();
    controller.dispatch(UiAction::SubmitBugReport);
    assert!(calls.lock().unwrap().is_empty());
    let popup = controller.view.bug_report_popup.as_mut().unwrap();
    popup.title = "Valid title".into();
    popup.body = "x".repeat(16_385);
    controller.dispatch(UiAction::SubmitBugReport);
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn bug_report_redacts_and_bounds_untrusted_capture_before_copy() {
    let (mut controller, calls) = bug_report_controller(false);
    controller.open_bug_report(Some(format!(
        "Authorization: Bearer screenshot-secret\n{}",
        "x".repeat(100_000)
    )));
    fill_bug_report(&mut controller);
    controller.dispatch(UiAction::CopyBugReport);
    let calls = calls.lock().unwrap();
    let DiagnosticCall::Copy(body) = &calls[0] else {
        panic!("copy only")
    };
    assert!(!body.contains("screenshot-secret"));
    assert!(body.len() < 40_000);
}

#[test]
fn bug_report_unknown_outcome_blocks_retry_and_keeps_diagnostics_independent() {
    let (mut controller, calls) = bug_report_controller(true);
    controller.report_actions = Box::new(MockDiagnosticActions {
        calls: Arc::clone(&calls),
        gh_available: true,
        submission_result: Mutex::new(Some(Err(GitHubIssueSubmissionFailure {
            message: "Connection lost; check existing issues".into(),
            outcome_unknown: true,
        }))),
    });
    controller.show_diagnostic_report("Existing failure", "existing report");
    let diagnostic = controller.view.error_popup.clone();
    controller.dispatch(UiAction::OpenBugReport);
    fill_bug_report(&mut controller);
    controller.dispatch(UiAction::SubmitBugReport);
    controller.dispatch(UiAction::RequestGitHubIssueSubmission);
    controller.dispatch(UiAction::ConfirmGitHubIssueSubmission);
    controller.tick();
    assert!(matches!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .submission,
        GitHubIssueSubmissionView::OutcomeUnknown { .. }
    ));
    controller.dispatch(UiAction::SubmitBugReport);
    assert_eq!(calls.lock().unwrap().len(), 1);
    controller.dispatch(UiAction::DismissBugReport);
    assert_eq!(controller.view.error_popup, diagnostic);
}

#[test]
fn bug_report_debug_never_exposes_draft_or_status_text() {
    let popup = BugReportPopupView {
        title: "secret title".into(),
        body: "secret draft".into(),
        validation_error: Some("secret failure".into()),
        submission: GitHubIssueSubmissionView::Failed {
            message: "secret response".into(),
        },
        ..BugReportPopupView::default()
    };
    assert!(!format!("{popup:?}").contains("secret"));
}

/// A definite rejection permits exactly one explicitly requested retry.
#[test]
fn bug_report_definite_failure_allows_edits_and_explicit_retry() {
    let (mut controller, calls) = bug_report_controller(true);
    controller.report_actions = Box::new(MockDiagnosticActions {
        calls: Arc::clone(&calls),
        gh_available: true,
        submission_result: Mutex::new(Some(Err(GitHubIssueSubmissionFailure {
            message: "Authentication required".into(),
            outcome_unknown: false,
        }))),
    });
    controller.dispatch(UiAction::OpenBugReport);
    fill_bug_report(&mut controller);
    controller.dispatch(UiAction::SubmitBugReport);
    controller.tick();
    assert!(matches!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .submission,
        GitHubIssueSubmissionView::Failed { .. }
    ));
    controller.dispatch(UiAction::AppendBugReportCharacter('!'));
    assert!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .body
            .ends_with('!')
    );
    controller.tick();
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "polling must never retry a POST"
    );
    controller.dispatch(UiAction::SubmitBugReport);
    controller.dispatch(UiAction::SubmitBugReport);
    assert_eq!(calls.lock().unwrap().len(), 2);
    controller.tick();
    assert!(matches!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .submission,
        GitHubIssueSubmissionView::Submitted { .. }
    ));
}

/// Holds mocked completions so tests can send stale worker ownership explicitly.
struct DeferredBugReportActions {
    submissions: Arc<Mutex<Vec<(u64, Sender<GitHubIssueSubmissionCompletion>)>>>,
}

impl DiagnosticActionHandler for DeferredBugReportActions {
    fn gh_available(&self) -> bool {
        true
    }
    fn copy_report(&self, _: &str) -> Result<String, String> {
        Ok("mock".into())
    }
    fn start_github_issue_submission(
        &self,
        _: String,
        _: String,
        generation: u64,
        results: Sender<GitHubIssueSubmissionCompletion>,
    ) -> Result<(), String> {
        self.submissions.lock().unwrap().push((generation, results));
        Ok(())
    }
    fn copy_and_open_github_issue(&self, _: &str, _: &str) -> Result<String, String> {
        panic!("manual reports must not require a browser")
    }
}

#[test]
fn bug_report_stale_worker_cannot_replace_a_new_pending_report() {
    let (mut controller, _) = bug_report_controller(true);
    let submissions = Arc::new(Mutex::new(Vec::new()));
    controller.report_actions = Box::new(DeferredBugReportActions {
        submissions: Arc::clone(&submissions),
    });
    controller.dispatch(UiAction::OpenBugReport);
    fill_bug_report(&mut controller);
    controller.dispatch(UiAction::SubmitBugReport);
    controller.tick();
    assert!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .animation_frame
            > 0
    );
    let (first_generation, first_sender) = submissions.lock().unwrap()[0].clone();
    first_sender
        .send(GitHubIssueSubmissionCompletion {
            generation: first_generation,
            result: Ok("https://github.com/vitaly-zdanevich/youta/issues/123".into()),
        })
        .unwrap();
    controller.tick();
    controller.dispatch(UiAction::DismissBugReport);
    controller.dispatch(UiAction::OpenBugReport);
    fill_bug_report(&mut controller);
    controller.dispatch(UiAction::SubmitBugReport);
    let (second_generation, second_sender) = submissions.lock().unwrap()[1].clone();
    assert_ne!(first_generation, second_generation);
    first_sender
        .send(GitHubIssueSubmissionCompletion {
            generation: first_generation,
            result: Err(GitHubIssueSubmissionFailure {
                message: "obsolete failure".into(),
                outcome_unknown: false,
            }),
        })
        .unwrap();
    controller.tick();
    assert_eq!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .submission,
        GitHubIssueSubmissionView::Submitting
    );
    controller.dispatch(UiAction::DismissBugReport);
    assert!(
        controller.view.bug_report_popup.is_some(),
        "stale worker cannot unlock the current pending form"
    );
    second_sender
        .send(GitHubIssueSubmissionCompletion {
            generation: second_generation,
            result: Ok("https://github.com/vitaly-zdanevich/youta/issues/124".into()),
        })
        .unwrap();
    controller.tick();
    assert_eq!(
        controller
            .view
            .bug_report_popup
            .as_ref()
            .unwrap()
            .submission,
        GitHubIssueSubmissionView::Submitted {
            url: "https://github.com/vitaly-zdanevich/youta/issues/124".into()
        }
    );
}
