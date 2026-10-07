//! RAM-only manual reports, independently owned from diagnostic report popups.

use super::*;
use crate::bug_report::{MAX_BODY_BYTES, compose_body, environment_footer, sanitize_screenshot};
use crate::diagnostics::redact_diagnostic_text;
use crate::report_actions::{MAX_ISSUE_TITLE_CHARS, bounded_issue_title};

/// Private capture and completion channel; screenshot text never enters the DTO.
pub(super) struct ManualBugReportState {
    screenshot: Option<String>,
    generation: u64,
    pub(super) pending: Option<u64>,
    sender: ResponseSender<GitHubIssueSubmissionCompletion>,
    results: Receiver<GitHubIssueSubmissionCompletion>,
}

impl Default for ManualBugReportState {
    fn default() -> Self {
        Self::new(WorkerNotifier::default())
    }
}

impl ManualBugReportState {
    /// Shares the controller's frontend notification without exposing report text.
    pub(super) fn new(notifier: WorkerNotifier) -> Self {
        let (sender, results) = unbounded();
        Self {
            screenshot: None,
            generation: 0,
            pending: None,
            sender: ResponseSender::new(sender, notifier),
            results,
        }
    }
}

/// Recognizes only actions owned by the topmost manual composer.
pub(super) fn composer_action(action: &UiAction) -> bool {
    matches!(
        action,
        UiAction::OpenBugReport
            | UiAction::SelectBugReportField(_)
            | UiAction::MoveBugReportField(_)
            | UiAction::AppendBugReportCharacter(_)
            | UiAction::DeleteBugReportCharacter
            | UiAction::DeleteBugReportForward
            | UiAction::DeleteBugReportWord
            | UiAction::MoveBugReportCursor(_)
            | UiAction::ToggleBugReportScreenshot
            | UiAction::SubmitBugReport
            | UiAction::CopyBugReport
            | UiAction::DismissBugReport
            | UiAction::OpenBugReportResult
    )
}

/// Editing is allowed before submission and after a definite, retryable failure.
fn editable(popup: &BugReportPopupView) -> bool {
    matches!(
        popup.submission,
        GitHubIssueSubmissionView::Idle | GitHubIssueSubmissionView::Failed { .. }
    )
}

/// Borrows the selected editable text and its grapheme-boundary cursor together.
fn selected_text(popup: &mut BugReportPopupView) -> Option<(&mut String, &mut usize)> {
    match popup.selected_field {
        BugReportField::Title => Some((&mut popup.title, &mut popup.title_cursor_byte)),
        BugReportField::Body => Some((&mut popup.body, &mut popup.body_cursor_byte)),
        BugReportField::Screenshot => None,
    }
}

impl AppController {
    /// Opens a new composer once, preserving all covered editor and diagnostic state.
    pub(super) fn open_bug_report_composer(&mut self, screenshot: Option<String>) {
        if self.view.bug_report_popup.is_some()
            || self.view.local_file_progress.is_some()
            || self.local_move_is_executing()
            || self.pending_github_issue_submission.is_some()
            || self.bug_report.pending.is_some()
        {
            return;
        }
        let screenshot_allowed = self.view.bug_report_screenshot_allowed();
        self.bug_report.screenshot = screenshot
            .filter(|_| screenshot_allowed)
            .map(|text| sanitize_screenshot(&text))
            .filter(|text| !text.is_empty());
        self.bug_report.generation = self.bug_report.generation.wrapping_add(1);
        let screenshot_available = self.bug_report.screenshot.is_some();
        let screenshot_notice = if !screenshot_allowed {
            Some("Screenshot omitted because a private or credential editor is open.".to_owned())
        } else if !screenshot_available {
            Some("No ASCII screenshot was available from this frontend.".to_owned())
        } else {
            None
        };
        self.view.bug_report_popup = Some(BugReportPopupView {
            screenshot_available,
            screenshot_notice,
            footer: environment_footer(),
            gh_available: self.report_actions.gh_available(),
            ..BugReportPopupView::default()
        });
    }

    /// Focuses a form field without changing the covered application's selection.
    pub(super) fn select_bug_report_field(&mut self, field: BugReportField) {
        if let Some(popup) = self
            .view
            .bug_report_popup
            .as_mut()
            .filter(|popup| editable(popup))
        {
            popup.selected_field = field;
            popup.follow_cursor = true;
        }
    }

    /// Cycles the two text inputs and screenshot checkbox with saturating-safe arithmetic.
    pub(super) fn move_bug_report_field(&mut self, direction: i32) {
        let Some(popup) = self
            .view
            .bug_report_popup
            .as_ref()
            .filter(|popup| editable(popup))
        else {
            return;
        };
        let current = match popup.selected_field {
            BugReportField::Title => 0,
            BugReportField::Body => 1,
            BugReportField::Screenshot => 2,
        };
        let field = match (current + i64::from(direction)).rem_euclid(3) {
            0 => BugReportField::Title,
            1 => BugReportField::Body,
            _ => BugReportField::Screenshot,
        };
        self.select_bug_report_field(field);
    }

    /// Inserts one bounded character while preserving Unicode grapheme cursor ownership.
    pub(super) fn append_bug_report_character(&mut self, character: char) {
        let Some(popup) = self
            .view
            .bug_report_popup
            .as_mut()
            .filter(|popup| editable(popup))
        else {
            return;
        };
        let body = popup.selected_field == BugReportField::Body;
        if character.is_control() && !(body && matches!(character, '\n' | '\t')) {
            return;
        }
        let Some((text, cursor)) = selected_text(popup) else {
            return;
        };
        if (body && text.len().saturating_add(character.len_utf8()) > MAX_BODY_BYTES)
            || (!body && text.chars().count() >= MAX_ISSUE_TITLE_CHARS)
        {
            popup.validation_error = Some(if body {
                format!(
                    "The body is limited to {MAX_BODY_BYTES} UTF-8 bytes; extra input was not inserted."
                )
            } else {
                format!(
                    "The title is limited to {MAX_ISSUE_TITLE_CHARS} characters; extra input was not inserted."
                )
            });
            return;
        }
        *cursor = editor_cursor_boundary(text, *cursor);
        text.insert(*cursor, character);
        let inserted_end = cursor.saturating_add(character.len_utf8());
        *cursor = text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .find(|index| *index >= inserted_end)
            .unwrap_or(text.len());
        popup.follow_cursor = true;
        popup.validation_error = None;
    }

    /// Removes one complete grapheme before or after the selected cursor.
    pub(super) fn delete_bug_report_character(&mut self, forward: bool) {
        let Some(popup) = self
            .view
            .bug_report_popup
            .as_mut()
            .filter(|popup| editable(popup))
        else {
            return;
        };
        let Some((text, cursor)) = selected_text(popup) else {
            return;
        };
        *cursor = editor_cursor_boundary(text, *cursor);
        let other = moved_private_note_cursor(
            text,
            *cursor,
            if forward {
                PrivateNoteCursorMotion::Right
            } else {
                PrivateNoteCursorMotion::Left
            },
        );
        let start = (*cursor).min(other);
        let end = (*cursor).max(other);
        text.replace_range(start..end, "");
        *cursor = start;
        popup.follow_cursor = true;
        popup.validation_error = None;
    }

    /// Reuses the shared word-deletion behavior without splitting UTF-8 or graphemes.
    pub(super) fn delete_bug_report_word(&mut self) {
        let Some(popup) = self
            .view
            .bug_report_popup
            .as_mut()
            .filter(|popup| editable(popup))
        else {
            return;
        };
        let Some((text, cursor)) = selected_text(popup) else {
            return;
        };
        delete_previous_editor_word(text, cursor);
        popup.follow_cursor = true;
        popup.validation_error = None;
    }

    /// Moves within the focused input using the existing multiline cursor rules.
    pub(super) fn move_bug_report_cursor(&mut self, motion: PrivateNoteCursorMotion) {
        let Some(popup) = self
            .view
            .bug_report_popup
            .as_mut()
            .filter(|popup| editable(popup))
        else {
            return;
        };
        let Some((text, cursor)) = selected_text(popup) else {
            return;
        };
        *cursor = moved_private_note_cursor(text, *cursor, motion);
        popup.follow_cursor = true;
    }

    /// Changes only whether the already captured screenshot enters the payload.
    pub(super) fn toggle_bug_report_screenshot(&mut self) {
        if let Some(popup) = self
            .view
            .bug_report_popup
            .as_mut()
            .filter(|popup| editable(popup) && popup.screenshot_available)
        {
            popup.with_screenshot = !popup.with_screenshot;
            popup.selected_field = BugReportField::Screenshot;
            popup.validation_error = None;
        }
    }

    /// Validates authored fields before copying or sending, without truncating them.
    fn bug_report_payload(&mut self) -> Option<(String, String)> {
        let popup = self.view.bug_report_popup.as_mut()?;
        let title = popup.title.trim();
        let error = if title.is_empty() {
            Some("Enter an issue title.".to_owned())
        } else if popup.title.chars().count() > MAX_ISSUE_TITLE_CHARS {
            Some(format!(
                "The title is limited to {MAX_ISSUE_TITLE_CHARS} characters."
            ))
        } else if popup.title.chars().any(char::is_control) {
            Some("The title must be a single line without control characters.".to_owned())
        } else if redact_diagnostic_text(title) != title {
            Some("Remove credentials or private paths from the issue title.".to_owned())
        } else if popup.body.trim().is_empty() {
            Some("Describe the problem before submitting.".to_owned())
        } else if popup.body.len() > MAX_BODY_BYTES {
            Some(format!(
                "The body is limited to {MAX_BODY_BYTES} UTF-8 bytes."
            ))
        } else {
            None
        };
        if let Some(error) = error {
            popup.validation_error = Some(error);
            return None;
        }
        let title = bounded_issue_title(title);
        popup.title.clone_from(&title);
        popup.title_cursor_byte = editor_cursor_boundary(&popup.title, popup.title_cursor_byte);
        let screenshot = self
            .bug_report
            .screenshot
            .as_deref()
            .filter(|_| popup.with_screenshot);
        Some((title, compose_body(&popup.body, screenshot, &popup.footer)))
    }

    /// Starts exactly one explicit POST, with no review or confirmation screen.
    pub(super) fn submit_bug_report(&mut self) {
        if self.bug_report.pending.is_some()
            || self.pending_github_issue_submission.is_some()
            || !self.view.bug_report_popup.as_ref().is_some_and(editable)
        {
            return;
        }
        let Some((title, report)) = self.bug_report_payload() else {
            return;
        };
        let popup = self
            .view
            .bug_report_popup
            .as_mut()
            .expect("validated composer");
        if !popup.gh_available {
            popup.validation_error = Some("Install GitHub CLI (gh), then run `gh auth login` in a terminal. Copy report remains available; no browser is required by Youta.".to_owned());
            return;
        }
        self.bug_report.generation = self.bug_report.generation.wrapping_add(1);
        let generation = self.bug_report.generation;
        self.bug_report.pending = Some(generation);
        popup.submission = GitHubIssueSubmissionView::Submitting;
        popup.animation_frame = 0;
        popup.validation_error = None;
        if let Err(error) = self.report_actions.start_github_issue_submission(
            title,
            report,
            generation,
            self.bug_report.sender.clone(),
        ) {
            self.bug_report.pending = None;
            popup.submission = GitHubIssueSubmissionView::Failed {
                message: format!("Could not start GitHub submission: {error}"),
            };
        }
    }

    /// Applies only this composer's completions and advances its pending spinner.
    pub(super) fn poll_bug_report_submission(&mut self) {
        if let Some(popup) = self.view.bug_report_popup.as_mut()
            && self.animation_tick_due
            && popup.submission == GitHubIssueSubmissionView::Submitting
        {
            popup.animation_frame = popup.animation_frame.wrapping_add(1);
        }
        while let Ok(completion) = self.bug_report.results.try_recv() {
            if self.bug_report.pending != Some(completion.generation) {
                continue;
            }
            self.bug_report.pending = None;
            let Some(popup) = self.view.bug_report_popup.as_mut() else {
                continue;
            };
            popup.submission = match completion.result {
                Ok(url) => GitHubIssueSubmissionView::Submitted { url },
                Err(error) if error.outcome_unknown => {
                    popup.validation_error = Some(error.message);
                    GitHubIssueSubmissionView::OutcomeUnknown {
                        issues_url: GITHUB_ISSUES_URL.to_owned(),
                    }
                }
                Err(error) => GitHubIssueSubmissionView::Failed {
                    message: format!(
                        "{}\nIf authentication is required, run `gh auth login` in a terminal. You can also copy this report.",
                        error.message
                    ),
                },
            };
        }
    }

    /// Copies the same frozen-footer payload without opening a browser or posting.
    pub(super) fn copy_bug_report(&mut self) {
        if self.bug_report.pending.is_some() {
            return;
        }
        let Some((_, report)) = self.bug_report_payload() else {
            return;
        };
        let status = match self.report_actions.copy_report(&report) {
            Ok(transport) => format!("Copied with {transport}"),
            Err(error) => format!("Copy failed: {error}"),
        };
        if let Some(popup) = self.view.bug_report_popup.as_mut() {
            popup.validation_error = Some(status);
        }
    }

    /// Drops one non-pending draft and reveals covered modals without changing them.
    pub(super) fn dismiss_bug_report(&mut self) {
        if self.bug_report.pending.is_some() {
            return;
        }
        self.view.bug_report_popup = None;
        self.bug_report.screenshot = None;
    }

    /// Opens only the validated issue URL retained after submission, when supported.
    pub(super) fn open_bug_report_result(&mut self) {
        let target =
            self.view
                .bug_report_popup
                .as_ref()
                .and_then(|popup| match &popup.submission {
                    GitHubIssueSubmissionView::Submitted { url } => Some(url.clone()),
                    GitHubIssueSubmissionView::OutcomeUnknown { issues_url } => {
                        Some(issues_url.clone())
                    }
                    _ => None,
                });
        if let Some(target) = target {
            self.open_external_url(&target);
        }
    }
}
