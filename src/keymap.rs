//! One keyboard map shared by every Youta front-end.
//!
//! The terminal owns an ordered chain of modal priorities in which the
//! first match consumes the key: an error popup outranks Preferences, which
//! outranks the playlist editor, and so on down to ordinary list navigation.
//! Restating that chain in a second language would guarantee the two
//! front-ends drift apart, so it lives here once, over a [`KeyPress`] that
//! names no terminal and no browser.
//!
//! A front-end's job is only to translate its own key events into [`KeyPress`]
//! and to report how much it rendered, through [`PopupGeometry`] and the
//! visible row count. It learns nothing about modality.

use serde::{Deserialize, Serialize};

use crate::config::SubscriptionsLayout;
use crate::domain::SourceKind;
use crate::subscriptions::SubscriptionKind;
use crate::view::*;

/// A key, named independently of any input source.
///
/// Function keys carry their number rather than one variant each, and every
/// printable key is a [`Key::Char`], so a front-end maps its own events without
/// this vocabulary growing a terminal or browser flavour.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Key {
    /// A printable character, already resolved for keyboard layout and Shift.
    Char(char),
    /// Return or Enter.
    Enter,
    /// Escape.
    Esc,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Tab.
    Tab,
    /// Back-tab, where the source reports it as a distinct key.
    BackTab,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// A function key, numbered from one.
    F(u8),
}

/// One key press with its modifier state.
///
/// Modifiers are three booleans rather than a bit set because this value
/// crosses a process boundary as JSON for out-of-process front-ends.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeyPress {
    /// The key itself.
    pub key: Key,
    /// Whether Control was held.
    pub ctrl: bool,
    /// Whether Alt, or Option, was held.
    pub alt: bool,
    /// Whether Shift was held.
    pub shift: bool,
}

impl KeyPress {
    /// Constructs an unmodified press.
    #[must_use]
    pub const fn new(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// Returns whether a chord modifier makes a printable key a command.
    ///
    /// Text editors inside Youta accept a character only when this is false,
    /// so `Ctrl+N` opens a playlist while `N` types a letter. Shift is absent
    /// deliberately: it produces the character rather than modifying it.
    #[must_use]
    pub const fn chorded(self) -> bool {
        self.ctrl || self.alt
    }

    /// Returns whether any modifier at all was held.
    ///
    /// Single-letter shortcuts that would otherwise shadow a capital or a
    /// chord require this to be false.
    #[must_use]
    pub const fn modified(self) -> bool {
        self.chorded() || self.shift
    }
}

/// Scroll state of one popup, in wrapped display lines.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScrollGeometry {
    /// First visible wrapped line.
    pub offset: usize,
    /// Largest offset that still shows content.
    pub maximum: usize,
    /// Wrapped lines one page step moves by.
    pub page_lines: usize,
}

/// What the front-end last rendered for the scrollable popups.
///
/// Paging depends on how much fits on screen, which only the front-end knows.
/// [`None`] means nothing has been rendered yet, and the modal popup keys are
/// then left alone rather than paged against a guess.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PopupGeometry {
    /// Local audio-quality progress/results popup.
    pub audio_quality: ScrollGeometry,
    /// Codex video-summary progress/results popup.
    pub video_summary: ScrollGeometry,
    /// Project-history popup.
    pub project_history: ScrollGeometry,
    /// Video-comments popup.
    pub video_comments: ScrollGeometry,
}

#[cfg(test)]
mod wire_tests {
    use super::{Key, KeyPress, PopupGeometry, key_action};
    use crate::playback::PlaybackStatus;
    #[cfg(feature = "commons-upload")]
    use crate::view::{CommonsUploadField, CommonsUploadPhase, CommonsUploadPopupView};
    #[cfg(feature = "evernote")]
    use crate::view::{EvernoteNoteField, EvernoteNotePhase, EvernoteNotePopupView};
    use crate::view::{UiAction, VideoSummaryPopupState, VideoSummaryPopupView, ViewModel};

    /// The window builds this JSON by hand in JavaScript, so the exact shape is
    /// part of the contract rather than an implementation detail of Serde.
    #[test]
    fn a_browser_key_event_deserializes_into_a_press() {
        let cases = [
            (
                r#"{"key":{"Char":"j"},"ctrl":false,"alt":false,"shift":false}"#,
                KeyPress::new(Key::Char('j')),
            ),
            (
                r#"{"key":"Enter","ctrl":false,"alt":false,"shift":false}"#,
                KeyPress::new(Key::Enter),
            ),
            (
                r#"{"key":"BackTab","ctrl":false,"alt":false,"shift":true}"#,
                KeyPress {
                    shift: true,
                    ..KeyPress::new(Key::BackTab)
                },
            ),
            (
                r#"{"key":{"F":8},"ctrl":false,"alt":false,"shift":false}"#,
                KeyPress::new(Key::F(8)),
            ),
            (
                r#"{"key":{"Char":"w"},"ctrl":true,"alt":false,"shift":false}"#,
                KeyPress {
                    ctrl: true,
                    ..KeyPress::new(Key::Char('w'))
                },
            ),
        ];
        for (json, expected) in cases {
            let decoded: KeyPress =
                serde_json::from_str(json).unwrap_or_else(|error| panic!("{json}: {error}"));
            assert_eq!(decoded, expected, "{json}");
        }
    }

    #[test]
    fn popup_geometry_survives_a_round_trip() {
        let geometry = PopupGeometry::default();
        let json = serde_json::to_string(&geometry).expect("encode geometry");
        assert_eq!(
            serde_json::from_str::<PopupGeometry>(&json).expect("decode geometry"),
            geometry
        );
    }

    /// A chord suppresses text entry; Shift alone must not, or capitals would
    /// stop reaching the search field.
    #[test]
    fn shift_alone_is_not_a_chord() {
        let shifted = KeyPress {
            shift: true,
            ..KeyPress::new(Key::Char('A'))
        };
        assert!(!shifted.chorded());
        assert!(shifted.modified());
        assert!(
            KeyPress {
                ctrl: true,
                ..shifted
            }
            .chorded()
        );
        assert!(
            KeyPress {
                alt: true,
                ..shifted
            }
            .chorded()
        );
    }

    #[test]
    fn bracket_keys_navigate_chapters_without_replacing_queue_navigation() {
        let finite = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyPress::new(Key::Char('[')), &finite, None, None),
            Some(UiAction::ChangeChapter(-1))
        );
        assert_eq!(
            key_action(KeyPress::new(Key::Char(']')), &finite, None, None),
            Some(UiAction::ChangeChapter(1))
        );
        assert_eq!(
            key_action(KeyPress::new(Key::Char('{')), &finite, None, None),
            Some(UiAction::PlayQueueNeighbour(-1))
        );
        assert_eq!(
            key_action(KeyPress::new(Key::Char('}')), &finite, None, None),
            Some(UiAction::PlayQueueNeighbour(1))
        );

        let live = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                live: true,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyPress::new(Key::Char('[')), &live, None, None),
            None
        );
        assert_eq!(
            key_action(KeyPress::new(Key::Char(']')), &live, None, None),
            None
        );

        for modified in [
            KeyPress {
                ctrl: true,
                ..KeyPress::new(Key::Char('['))
            },
            KeyPress {
                alt: true,
                ..KeyPress::new(Key::Char(']'))
            },
            KeyPress {
                shift: true,
                ..KeyPress::new(Key::Char('['))
            },
        ] {
            assert_eq!(
                key_action(modified, &finite, None, None),
                None,
                "chapter navigation is reserved for bare bracket keys"
            );
        }
    }

    #[test]
    fn uppercase_g_opens_only_an_available_video_summary() {
        let mut view = ViewModel::default();
        assert_eq!(
            key_action(KeyPress::new(Key::Char('G')), &view, None, None),
            None
        );
        view.video_summary_available = true;
        assert_eq!(
            key_action(KeyPress::new(Key::Char('G')), &view, None, None),
            Some(UiAction::GenerateVideoSummary)
        );
    }

    #[cfg(feature = "commons-upload")]
    #[test]
    fn uppercase_u_opens_only_an_available_commons_upload() {
        let mut view = ViewModel::default();
        assert_eq!(
            key_action(KeyPress::new(Key::Char('U')), &view, None, None),
            None
        );
        view.commons_upload_available = true;
        assert_eq!(
            key_action(KeyPress::new(Key::Char('U')), &view, None, None),
            Some(UiAction::OpenCommonsUpload)
        );
        assert_eq!(
            key_action(KeyPress::new(Key::Char('u')), &view, None, None),
            Some(UiAction::OpenQueuePopup),
            "the existing lowercase queue shortcut must remain unchanged"
        );
    }

    #[cfg(feature = "commons-upload")]
    #[test]
    fn commons_review_owns_text_category_and_submit_keys() {
        let popup = CommonsUploadPopupView {
            selected_field: CommonsUploadField::Description,
            ..CommonsUploadPopupView::default()
        };
        let mut view = ViewModel {
            commons_upload_popup: Some(popup),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyPress::new(Key::Enter), &view, None, None),
            Some(UiAction::InsertCommonsUploadNewline)
        );
        assert_eq!(
            key_action(
                KeyPress {
                    ctrl: true,
                    ..KeyPress::new(Key::Char('s'))
                },
                &view,
                None,
                None,
            ),
            Some(UiAction::SubmitCommonsUpload)
        );

        view.commons_upload_popup
            .as_mut()
            .expect("Commons popup")
            .phase = CommonsUploadPhase::Complete;
        assert_eq!(
            key_action(KeyPress::new(Key::Enter), &view, None, None),
            Some(UiAction::OpenCommonsUploadResult)
        );
    }

    #[cfg(feature = "evernote")]
    #[test]
    fn uppercase_e_opens_only_an_available_evernote_export() {
        let mut view = ViewModel::default();
        assert_eq!(
            key_action(KeyPress::new(Key::Char('E')), &view, None, None),
            None
        );
        view.evernote_available = true;
        assert_eq!(
            key_action(KeyPress::new(Key::Char('E')), &view, None, None),
            Some(UiAction::OpenEvernoteNote)
        );
    }

    #[cfg(feature = "evernote")]
    #[test]
    fn evernote_body_owns_newline_undo_captions_and_submit_keys() {
        let popup = EvernoteNotePopupView {
            selected_field: EvernoteNoteField::Body,
            captions_available: true,
            undo_available: true,
            ..EvernoteNotePopupView::default()
        };
        let mut view = ViewModel {
            evernote_popup: Some(popup),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyPress::new(Key::Enter), &view, None, None),
            Some(UiAction::InsertEvernoteNoteNewline)
        );
        assert_eq!(
            key_action(
                KeyPress {
                    ctrl: true,
                    ..KeyPress::new(Key::Char('z'))
                },
                &view,
                None,
                None,
            ),
            Some(UiAction::UndoEvernoteNoteBody)
        );
        assert_eq!(
            key_action(KeyPress::new(Key::F(2)), &view, None, None),
            Some(UiAction::InsertEvernoteCaptions)
        );
        assert_eq!(
            key_action(
                KeyPress {
                    ctrl: true,
                    ..KeyPress::new(Key::Char('s'))
                },
                &view,
                None,
                None,
            ),
            Some(UiAction::SubmitEvernoteNote)
        );

        view.evernote_popup.as_mut().expect("Evernote popup").phase = EvernoteNotePhase::Complete;
        assert_eq!(
            key_action(KeyPress::new(Key::Enter), &view, None, None),
            Some(UiAction::OpenEvernoteNoteResult)
        );
    }

    #[test]
    fn video_summary_popup_owns_cancel_copy_and_rendered_paging() {
        let mut view = ViewModel {
            video_summary_popup: Some(VideoSummaryPopupView::default()),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyPress::new(Key::Esc), &view, None, None),
            Some(UiAction::CancelVideoSummary)
        );

        view.video_summary_popup = Some(VideoSummaryPopupView {
            state: VideoSummaryPopupState::Ready,
            report: "summary".to_owned(),
            ..VideoSummaryPopupView::default()
        });
        assert_eq!(
            key_action(KeyPress::new(Key::Char('c')), &view, None, None),
            Some(UiAction::CopyVideoSummary)
        );
        let mut geometry = PopupGeometry::default();
        geometry.video_summary.offset = 3;
        geometry.video_summary.maximum = 20;
        geometry.video_summary.page_lines = 7;
        assert_eq!(
            key_action(KeyPress::new(Key::PageDown), &view, None, Some(geometry),),
            Some(UiAction::SetVideoSummaryScroll(10))
        );
    }
}

/// Returns whether one key is the editor-local Vim word-delete chord.
fn is_delete_previous_word_key(key: KeyPress) -> bool {
    key.ctrl && !key.alt && matches!(key.key, Key::Char('w' | 'W'))
}

/// Returns whether subscription item controls own the current keyboard focus.
fn subscription_items_active(view: &ViewModel) -> bool {
    view.screen == Screen::Subscriptions
        && (view.subscriptions.route == SubscriptionRoute::Items
            || (view.subscriptions.layout == SubscriptionsLayout::Split
                && view.subscriptions.focus == SubscriptionPane::Items))
}

/// Maps one key using the current rendered main-list page capacity.
pub fn key_action(
    key: KeyPress,
    view: &ViewModel,
    page_rows: Option<usize>,
    popups: Option<PopupGeometry>,
) -> Option<UiAction> {
    if view.error_popup.is_none()
        && let Some(popup) = view.audio_quality_popup.as_ref()
    {
        return if let Some(popups) = popups {
            audio_quality_key_action(
                key,
                popup,
                popups.audio_quality.offset,
                popups.audio_quality.maximum,
                popups.audio_quality.page_lines,
            )
        } else {
            audio_quality_control_action(key, popup)
        };
    }
    if view.error_popup.is_none()
        && let Some(popup) = view.video_summary_popup.as_ref()
    {
        return if let Some(popups) = popups {
            video_summary_key_action(
                key,
                popup,
                popups.video_summary.offset,
                popups.video_summary.maximum,
                popups.video_summary.page_lines,
            )
        } else {
            video_summary_control_action(key, popup)
        };
    }
    if view.error_popup.is_none()
        && view.project_history_popup.is_some()
        && let Some(popups) = popups
    {
        return project_history_key_action(
            key,
            popups.project_history.offset,
            popups.project_history.maximum,
            popups.project_history.page_lines,
        );
    }
    if view.error_popup.is_none()
        && view.video_comments_popup.is_some()
        && let Some(popups) = popups
    {
        return video_comments_key_action(
            key,
            popups.video_comments.offset,
            popups.video_comments.maximum,
            popups.video_comments.page_lines,
        );
    }
    unfiltered_key_action(key, view, page_rows)
        .filter(|action| view.external_opener_available || !action.requires_external_opener())
}

/// Maps modal local audio-quality report controls.
fn audio_quality_key_action(
    key: KeyPress,
    popup: &AudioQualityPopupView,
    offset: usize,
    maximum: usize,
    page_lines: usize,
) -> Option<UiAction> {
    if let Some(action) = audio_quality_control_action(key, popup) {
        return Some(action);
    }
    let page_lines = page_lines.max(1);
    match key.key {
        Key::Up | Key::Left | Key::Char('k') => Some(UiAction::SetAudioQualityPopupScroll(
            offset.saturating_sub(1),
        )),
        Key::Down | Key::Right | Key::Char('j') => Some(UiAction::SetAudioQualityPopupScroll(
            offset.saturating_add(1).min(maximum),
        )),
        Key::PageUp => Some(UiAction::SetAudioQualityPopupScroll(
            offset.saturating_sub(page_lines),
        )),
        Key::PageDown => Some(UiAction::SetAudioQualityPopupScroll(
            offset.saturating_add(page_lines).min(maximum),
        )),
        Key::Home => Some(UiAction::SetAudioQualityPopupScroll(0)),
        Key::End => Some(UiAction::SetAudioQualityPopupScroll(maximum)),
        _ => None,
    }
}

/// Maps audio-quality controls that do not depend on rendered geometry.
fn audio_quality_control_action(key: KeyPress, popup: &AudioQualityPopupView) -> Option<UiAction> {
    match key.key {
        Key::Esc if popup.pending => Some(UiAction::CancelAudioQualityAnalysis),
        Key::Esc => Some(UiAction::DismissAudioQualityPopup),
        Key::Char('c' | 'C') if !popup.report.is_empty() => Some(UiAction::CopyAudioQualityReport),
        _ => None,
    }
}

/// Maps modal video-summary report controls.
fn video_summary_key_action(
    key: KeyPress,
    popup: &VideoSummaryPopupView,
    offset: usize,
    maximum: usize,
    page_lines: usize,
) -> Option<UiAction> {
    if let Some(action) = video_summary_control_action(key, popup) {
        return Some(action);
    }
    let page_lines = page_lines.max(1);
    match key.key {
        Key::Up | Key::Left | Key::Char('k') => {
            Some(UiAction::SetVideoSummaryScroll(offset.saturating_sub(1)))
        }
        Key::Down | Key::Right | Key::Char('j') => Some(UiAction::SetVideoSummaryScroll(
            offset.saturating_add(1).min(maximum),
        )),
        Key::PageUp => Some(UiAction::SetVideoSummaryScroll(
            offset.saturating_sub(page_lines),
        )),
        Key::PageDown => Some(UiAction::SetVideoSummaryScroll(
            offset.saturating_add(page_lines).min(maximum),
        )),
        Key::Home => Some(UiAction::SetVideoSummaryScroll(0)),
        Key::End => Some(UiAction::SetVideoSummaryScroll(maximum)),
        _ => None,
    }
}

/// Maps video-summary controls that do not depend on rendered geometry.
fn video_summary_control_action(key: KeyPress, popup: &VideoSummaryPopupView) -> Option<UiAction> {
    match key.key {
        Key::Esc if popup.state.pending() => Some(UiAction::CancelVideoSummary),
        Key::Esc => Some(UiAction::DismissVideoSummary),
        Key::Char('c' | 'C') if !popup.report.is_empty() => Some(UiAction::CopyVideoSummary),
        _ => None,
    }
}

/// Maps modal project-history navigation to one resize-aware wrapped-line offset.
pub(crate) fn project_history_key_action(
    key: KeyPress,
    offset: usize,
    maximum: usize,
    page_lines: usize,
) -> Option<UiAction> {
    let page_lines = page_lines.max(1);
    match key.key {
        Key::Esc | Key::F(9) => Some(UiAction::DismissProjectHistory),
        Key::Up | Key::Char('k') => {
            Some(UiAction::SetProjectHistoryScroll(offset.saturating_sub(1)))
        }
        Key::Down | Key::Char('j') => Some(UiAction::SetProjectHistoryScroll(
            offset.saturating_add(1).min(maximum),
        )),
        Key::PageUp => Some(UiAction::SetProjectHistoryScroll(
            offset.saturating_sub(page_lines),
        )),
        Key::PageDown => Some(UiAction::SetProjectHistoryScroll(
            offset.saturating_add(page_lines).min(maximum),
        )),
        Key::Home => Some(UiAction::SetProjectHistoryScroll(0)),
        Key::End => Some(UiAction::SetProjectHistoryScroll(maximum)),
        _ => None,
    }
}

/// Maps modal comments navigation to one resize-aware wrapped-line offset.
pub(crate) fn video_comments_key_action(
    key: KeyPress,
    offset: usize,
    maximum: usize,
    page_lines: usize,
) -> Option<UiAction> {
    let page_lines = page_lines.max(1);
    match key.key {
        Key::Esc | Key::F(6) => Some(UiAction::DismissVideoComments),
        Key::Up | Key::Char('k') => {
            Some(UiAction::SetVideoCommentsScroll(offset.saturating_sub(1)))
        }
        Key::Down | Key::Char('j') => Some(UiAction::SetVideoCommentsScroll(
            offset.saturating_add(1).min(maximum),
        )),
        Key::PageUp => Some(UiAction::SetVideoCommentsScroll(
            offset.saturating_sub(page_lines),
        )),
        Key::PageDown => Some(UiAction::SetVideoCommentsScroll(
            offset.saturating_add(page_lines).min(maximum),
        )),
        Key::Home => Some(UiAction::SetVideoCommentsScroll(0)),
        Key::End => Some(UiAction::SetVideoCommentsScroll(maximum)),
        _ => None,
    }
}

/// Maps one key before applying terminal-capability policy.
fn unfiltered_key_action(
    key: KeyPress,
    view: &ViewModel,
    page_rows: Option<usize>,
) -> Option<UiAction> {
    if let Some(error) = view.error_popup.as_ref() {
        let yt_dlp_forbidden = error.yt_dlp_forbidden.as_ref();
        if yt_dlp_forbidden.is_none() && error.reportable {
            match &error.github_issue_submission {
                GitHubIssueSubmissionView::Confirming => {
                    return match key.key {
                        Key::Enter => Some(UiAction::ConfirmGitHubIssueSubmission),
                        Key::Esc => Some(UiAction::CancelGitHubIssueSubmission),
                        Key::Char('c' | 'C') => Some(UiAction::CopyErrorReport),
                        Key::Up | Key::Left => {
                            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(-1)))
                        }
                        Key::Down | Key::Right => {
                            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(1)))
                        }
                        Key::PageUp => {
                            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(-1)))
                        }
                        Key::PageDown => {
                            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(1)))
                        }
                        Key::Home => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Home)),
                        Key::End => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::End)),
                        _ => None,
                    };
                }
                GitHubIssueSubmissionView::Submitting => return None,
                GitHubIssueSubmissionView::Submitted { .. }
                | GitHubIssueSubmissionView::OutcomeUnknown { .. } => {
                    return match key.key {
                        Key::Esc => Some(UiAction::DismissErrorPopup),
                        Key::Char('c' | 'C') => Some(UiAction::CopyErrorReport),
                        Key::Char('o' | 'O') => Some(UiAction::OpenGitHubIssueSubmissionTarget),
                        Key::Up | Key::Left => {
                            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(-1)))
                        }
                        Key::Down | Key::Right => {
                            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(1)))
                        }
                        Key::PageUp => {
                            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(-1)))
                        }
                        Key::PageDown => {
                            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(1)))
                        }
                        Key::Home => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Home)),
                        Key::End => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::End)),
                        _ => None,
                    };
                }
                GitHubIssueSubmissionView::Idle | GitHubIssueSubmissionView::Failed { .. } => {}
            }
        }
        return match key.key {
            Key::Esc => Some(UiAction::DismissErrorPopup),
            Key::Char('c' | 'C') => Some(UiAction::CopyErrorReport),
            Key::Char('u' | 'U') if yt_dlp_forbidden.is_some() => Some(UiAction::OpenYtDlpProject),
            Key::Char('p' | 'P') if yt_dlp_forbidden.is_some_and(|view| view.gentoo.is_some()) => {
                Some(UiAction::OpenGentooYtDlpPackage)
            }
            Key::Char('g' | 'G')
                if yt_dlp_forbidden.is_none() && error.reportable && error.gh_available =>
            {
                Some(UiAction::RequestGitHubIssueSubmission)
            }
            Key::Char('i' | 'I') if yt_dlp_forbidden.is_none() && error.reportable => {
                Some(UiAction::CopyAndOpenGitHubIssue)
            }
            Key::Up | Key::Left => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(-1))),
            Key::Down | Key::Right => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(1))),
            Key::PageUp => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(-1))),
            Key::PageDown => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(1))),
            Key::Home => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Home)),
            Key::End => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::End)),
            _ => None,
        };
    }
    #[cfg(feature = "qr")]
    {
        if view.video_qr_popup.is_some() {
            return match key.key {
                Key::Esc | Key::Char('Q') => Some(UiAction::DismissVideoQr),
                _ => None,
            };
        }
    }
    if let Some(popup) = view.project_history_popup.as_ref() {
        return project_history_key_action(key, popup.scroll_offset, usize::MAX, 20);
    }
    if let Some(popup) = view.video_comments_popup.as_ref() {
        return video_comments_key_action(key, popup.scroll_offset, usize::MAX, 20);
    }
    #[cfg(feature = "commons-upload")]
    if let Some(popup) = view.commons_upload_popup.as_ref() {
        if popup.phase == CommonsUploadPhase::Complete {
            return match key.key {
                Key::Enter => Some(UiAction::OpenCommonsUploadResult),
                Key::Esc => Some(UiAction::DismissCommonsUpload),
                _ => None,
            };
        }
        if popup.phase != CommonsUploadPhase::Review {
            return match key.key {
                Key::Esc => Some(UiAction::DismissCommonsUpload),
                _ => None,
            };
        }
        let fields = [
            CommonsUploadField::Title,
            CommonsUploadField::Caption,
            CommonsUploadField::Description,
            CommonsUploadField::Source,
            CommonsUploadField::Author,
            CommonsUploadField::Category,
        ];
        let selected = fields
            .iter()
            .position(|field| *field == popup.selected_field)
            .unwrap_or_default();
        let next = fields[(selected + 1) % fields.len()];
        let previous = fields[(selected + fields.len() - 1) % fields.len()];
        if key.key == Key::Tab {
            return Some(UiAction::SelectCommonsUploadField(if reverse_tab(key) {
                previous
            } else {
                next
            }));
        }
        return match key.key {
            Key::Esc => Some(UiAction::DismissCommonsUpload),
            Key::Char('s' | 'S') if key.ctrl => Some(UiAction::SubmitCommonsUpload),
            Key::Up if popup.selected_field == CommonsUploadField::Category => {
                Some(UiAction::MoveCommonsCategorySuggestion(-1))
            }
            Key::Down if popup.selected_field == CommonsUploadField::Category => {
                Some(UiAction::MoveCommonsCategorySuggestion(1))
            }
            Key::BackTab | Key::Up => Some(UiAction::SelectCommonsUploadField(previous)),
            Key::Enter
                if popup.selected_field == CommonsUploadField::Category
                    && !popup.category_suggestions.is_empty() =>
            {
                Some(UiAction::AddCommonsCategorySuggestion)
            }
            Key::Enter if popup.selected_field == CommonsUploadField::Description => {
                Some(UiAction::InsertCommonsUploadNewline)
            }
            Key::Down | Key::Enter => Some(UiAction::SelectCommonsUploadField(next)),
            Key::Char('l') if !key.chorded() => Some(UiAction::CycleCommonsUploadLicense),
            Key::Char('o') if popup.selected_field == CommonsUploadField::Category => {
                Some(UiAction::OpenCommonsCategorySuggestion)
            }
            Key::Char('x')
                if popup.selected_field == CommonsUploadField::Category
                    && popup.category_query.is_empty()
                    && !popup.draft.categories.is_empty() =>
            {
                Some(UiAction::RemoveCommonsUploadCategory(
                    popup.draft.categories.len() - 1,
                ))
            }
            Key::Backspace => Some(UiAction::DeleteCommonsUploadCharacter),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeleteCommonsUploadWord)
            }
            Key::Char(character) if !character.is_control() && !key.chorded() => {
                Some(UiAction::AppendCommonsUploadCharacter(character))
            }
            _ => None,
        };
    }
    #[cfg(feature = "evernote")]
    if let Some(popup) = view.evernote_popup.as_ref() {
        if popup.phase == EvernoteNotePhase::Complete {
            return match key.key {
                Key::Enter => Some(UiAction::OpenEvernoteNoteResult),
                Key::Esc => Some(UiAction::DismissEvernoteNote),
                _ => None,
            };
        }
        if popup.phase != EvernoteNotePhase::Review {
            return match key.key {
                Key::Esc => Some(UiAction::DismissEvernoteNote),
                _ => None,
            };
        }
        let fields = [
            EvernoteNoteField::Title,
            EvernoteNoteField::Body,
            EvernoteNoteField::Tags,
        ];
        let selected = fields
            .iter()
            .position(|field| *field == popup.selected_field)
            .unwrap_or_default();
        let next = fields[(selected + 1) % fields.len()];
        let previous = fields[(selected + fields.len() - 1) % fields.len()];
        if key.key == Key::Tab {
            return Some(UiAction::SelectEvernoteNoteField(if reverse_tab(key) {
                previous
            } else {
                next
            }));
        }
        return match key.key {
            Key::Esc => Some(UiAction::DismissEvernoteNote),
            Key::Char('s' | 'S') if key.ctrl => Some(UiAction::SubmitEvernoteNote),
            Key::Char('z' | 'Z')
                if key.ctrl
                    && popup.selected_field == EvernoteNoteField::Body
                    && popup.undo_available =>
            {
                Some(UiAction::UndoEvernoteNoteBody)
            }
            Key::F(2) if popup.captions_available => Some(UiAction::InsertEvernoteCaptions),
            Key::BackTab | Key::Up => Some(UiAction::SelectEvernoteNoteField(previous)),
            Key::Enter if popup.selected_field == EvernoteNoteField::Body => {
                Some(UiAction::InsertEvernoteNoteNewline)
            }
            Key::Down | Key::Enter => Some(UiAction::SelectEvernoteNoteField(next)),
            Key::Backspace => Some(UiAction::DeleteEvernoteNoteCharacter),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeleteEvernoteNoteWord)
            }
            Key::Char(character) if !character.is_control() && !key.chorded() => {
                Some(UiAction::AppendEvernoteNoteCharacter(character))
            }
            _ => None,
        };
    }
    #[cfg(feature = "commons-upload")]
    if view.commons_credentials_popup.is_some() {
        return match key.key {
            Key::Esc => Some(UiAction::DismissCommonsCredentials),
            Key::Enter => Some(UiAction::SubmitCommonsCredentials),
            Key::Tab | Key::BackTab | Key::Up | Key::Down => {
                Some(UiAction::ToggleCommonsCredentialField)
            }
            Key::F(1) => Some(UiAction::OpenCommonsBotPasswordGuide),
            Key::F(2) => Some(UiAction::OpenCommonsAccountRegistration),
            Key::F(3) => Some(UiAction::CycleCommonsAuthMethod),
            Key::Backspace => Some(UiAction::DeleteCommonsCredentialCharacter),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeleteCommonsCredentialWord)
            }
            Key::Char(character) if !character.is_control() && !key.chorded() => {
                Some(UiAction::AppendCommonsCredentialCharacter(character))
            }
            _ => None,
        };
    }
    #[cfg(feature = "evernote")]
    if view.evernote_credentials_popup.is_some() {
        return match key.key {
            Key::Esc => Some(UiAction::DismissEvernoteCredentials),
            Key::Enter => Some(UiAction::SubmitEvernoteCredentials),
            Key::F(1) => Some(UiAction::OpenEvernoteDeveloperTokenGuide),
            Key::Backspace => Some(UiAction::DeleteEvernoteTokenCharacter),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeleteEvernoteTokenWord)
            }
            Key::Char(character) if !character.is_control() && !key.chorded() => {
                Some(UiAction::AppendEvernoteTokenCharacter(character))
            }
            _ => None,
        };
    }
    if let Some(popup) = view.private_note_popup.as_ref() {
        let control = key.ctrl;
        return match key.key {
            Key::Esc => Some(UiAction::DismissPrivateNotePopup),
            Key::Char('s' | 'S') if control => Some(UiAction::SavePrivateNote),
            Key::Delete => Some(UiAction::RequestPrivateNoteDelete),
            Key::Enter if popup.confirming_delete => Some(UiAction::RequestPrivateNoteDelete),
            Key::Enter => Some(UiAction::InsertPrivateNoteNewline),
            Key::Backspace => Some(UiAction::DeletePrivateNoteCharacter),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeletePrivateNoteWord)
            }
            Key::Left => Some(UiAction::MovePrivateNoteCursor(
                PrivateNoteCursorMotion::Left,
            )),
            Key::Right => Some(UiAction::MovePrivateNoteCursor(
                PrivateNoteCursorMotion::Right,
            )),
            Key::Up => Some(UiAction::MovePrivateNoteCursor(PrivateNoteCursorMotion::Up)),
            Key::Down => Some(UiAction::MovePrivateNoteCursor(
                PrivateNoteCursorMotion::Down,
            )),
            Key::Home => Some(UiAction::MovePrivateNoteCursor(
                PrivateNoteCursorMotion::Home,
            )),
            Key::End => Some(UiAction::MovePrivateNoteCursor(
                PrivateNoteCursorMotion::End,
            )),
            Key::Char(character) if !character.is_control() && !key.chorded() => {
                Some(UiAction::AppendPrivateNoteCharacter(character))
            }
            _ => None,
        };
    }
    if let Some(popup) = view.playlist_popup.as_ref() {
        return match popup.mode {
            PlaylistPopupMode::Choose => match key.key {
                Key::Esc => Some(UiAction::DismissPlaylistPopup),
                Key::Enter => Some(UiAction::ToggleSelectedPlaylistMembership),
                Key::Up | Key::Char('k') => Some(UiAction::MovePlaylistPopupSelection(-1)),
                Key::Down | Key::Char('j') => Some(UiAction::MovePlaylistPopupSelection(1)),
                Key::Char('n') if !key.chorded() => Some(UiAction::BeginNewPlaylist),
                _ => None,
            },
            PlaylistPopupMode::Create | PlaylistPopupMode::Edit => match key.key {
                Key::Esc => Some(UiAction::DismissPlaylistPopup),
                Key::Enter if popup.mode == PlaylistPopupMode::Create => {
                    Some(UiAction::CreatePlaylistAndAdd)
                }
                Key::Enter => Some(UiAction::UpdatePlaylist),
                Key::Tab | Key::BackTab => Some(UiAction::SelectPlaylistEditorField(
                    match popup.editor_field {
                        PlaylistEditorField::Name => PlaylistEditorField::Description,
                        PlaylistEditorField::Description => PlaylistEditorField::Name,
                    },
                )),
                Key::Up => Some(UiAction::SelectPlaylistEditorField(
                    PlaylistEditorField::Name,
                )),
                Key::Down => Some(UiAction::SelectPlaylistEditorField(
                    PlaylistEditorField::Description,
                )),
                Key::Backspace => Some(UiAction::DeletePlaylistEditorCharacter),
                Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                    Some(UiAction::DeletePlaylistEditorWord)
                }
                Key::Char(character) if !character.is_control() && !key.chorded() => {
                    Some(UiAction::AppendPlaylistEditorCharacter(character))
                }
                _ => None,
            },
        };
    }
    if let Some(popup) = view.queue_popup.as_ref() {
        return match key.key {
            Key::Esc | Key::Char('u') => Some(UiAction::DismissQueuePopup),
            Key::Up | Key::Char('k') => Some(UiAction::MoveQueuePopupSelection(-1)),
            Key::Down | Key::Char('j') => Some(UiAction::MoveQueuePopupSelection(1)),
            Key::Enter => Some(UiAction::ActivateQueuePopupRow(popup.selected)),
            // `x` rather than Delete alone: Delete is the Local trash key, and a
            // key that destroys a file on one screen must not be the reflex for
            // dropping a queue entry on another.
            Key::Char('x') | Key::Delete => Some(UiAction::RemoveQueuePopupRow(popup.selected)),
            Key::Char('C') => Some(UiAction::ClearQueue),
            _ => None,
        };
    }
    if let Some(popup) = view.local_file_popup.as_ref() {
        return match (popup, key.key) {
            (_, Key::Esc) => Some(UiAction::DismissLocalFilePopup),
            (LocalFilePopupView::Rename { .. }, Key::Enter) => Some(UiAction::SubmitLocalRename),
            (LocalFilePopupView::Rename { .. }, Key::Backspace) => {
                Some(UiAction::DeleteLocalRenameCharacter)
            }
            (LocalFilePopupView::Rename { .. }, Key::Char('w' | 'W'))
                if is_delete_previous_word_key(key) =>
            {
                Some(UiAction::DeleteLocalRenameWord)
            }
            (LocalFilePopupView::Rename { .. }, Key::Left) => {
                Some(UiAction::MoveLocalRenameCursor(-1))
            }
            (LocalFilePopupView::Rename { .. }, Key::Right) => {
                Some(UiAction::MoveLocalRenameCursor(1))
            }
            (LocalFilePopupView::Rename { .. }, Key::Char(character))
                if !character.is_control() && !key.chorded() =>
            {
                Some(UiAction::AppendLocalRenameCharacter(character))
            }
            (LocalFilePopupView::Trash { .. }, Key::Enter) => Some(UiAction::ConfirmLocalTrash),
            (LocalFilePopupView::DownloadedTrash { .. }, Key::Enter) => {
                Some(UiAction::ConfirmDownloadedTrash)
            }
            (LocalFilePopupView::Move { .. }, Key::Enter) => {
                Some(UiAction::ActivateLocalMoveDestination)
            }
            (LocalFilePopupView::Move { .. }, Key::Char('m' | 'M')) => {
                Some(UiAction::ConfirmLocalMoveHere)
            }
            (LocalFilePopupView::Move { .. }, Key::Up | Key::Char('k')) => {
                Some(UiAction::MoveLocalMoveDestination(-1))
            }
            (LocalFilePopupView::Move { .. }, Key::Down | Key::Char('j')) => {
                Some(UiAction::MoveLocalMoveDestination(1))
            }
            _ => None,
        };
    }
    if view.rss_subscription_popup.is_some() {
        return match key.key {
            Key::Esc => Some(UiAction::DismissRssSubscriptionPopup),
            Key::Enter => Some(UiAction::SubmitRssSubscription),
            Key::Backspace => Some(UiAction::DeleteRssSubscriptionCharacter),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeleteRssSubscriptionWord)
            }
            Key::Char(character) if !character.is_control() && !key.chorded() => {
                Some(UiAction::AppendRssSubscriptionCharacter(character))
            }
            _ => None,
        };
    }
    if let Some(preferences) = view.preferences_popup.as_ref() {
        let alternative = preferences.subscriptions_layout.toggled();
        return match key.key {
            Key::Esc | Key::Char('p') => Some(UiAction::DismissPreferences),
            Key::Enter => Some(UiAction::SubmitPreferences),
            Key::Char('a') => Some(UiAction::ToggleSkipAdvertisementChapters),
            Key::Char('y') => Some(UiAction::ToggleYouTubePrewarm),
            Key::Char('t') if cfg!(feature = "images") => Some(UiAction::CycleYouTubeThumbnailSize),
            Key::Char('f') => Some(UiAction::ToggleLocalFolderSizes),
            Key::Char('i') if cfg!(feature = "images") => Some(UiAction::ToggleTtyImages),
            Key::Char('b') if cfg!(feature = "bandcamp") => {
                Some(UiAction::CycleBandcampAudioFormat)
            }
            Key::Char('c') if preferences.video_summary_supported => {
                Some(UiAction::CycleVideoSummaryBackend)
            }
            Key::Char('d') => Some(UiAction::SetSubscriptionsLayout(
                SubscriptionsLayout::DrillDown,
            )),
            Key::Char('s') => Some(UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split)),
            Key::Left | Key::Right | Key::Up | Key::Down | Key::Char(' ') => {
                Some(UiAction::SetSubscriptionsLayout(alternative))
            }
            _ => None,
        };
    }
    if view.text_selection_mode
        && !view
            .details
            .as_ref()
            .is_some_and(|details| details.thumbnail_expanded)
    {
        let control = key.ctrl;
        let shift = key.shift;
        if control && matches!(key.key, Key::Char('c' | 'C')) {
            // Terminals normally consume Ctrl+Shift+C as their Copy command.
            // If one forwards it, do not reinterpret that copy chord as Quit.
            return (!shift).then_some(UiAction::Quit);
        }
        return match key.key {
            Key::Esc | Key::Char('t') => Some(UiAction::ToggleTextSelectionMode),
            Key::Char('T') => Some(UiAction::ToggleChapterTimestamps),
            _ => None,
        };
    }
    if let Some(setup) = view.yandex_music_setup_popup.as_ref() {
        if setup.validating {
            return match key.key {
                Key::Esc => Some(UiAction::DismissYandexMusicSetup),
                Key::F(1) => Some(UiAction::OpenYandexOAuthGuide),
                _ => None,
            };
        }
        return match key.key {
            Key::Esc => Some(UiAction::DismissYandexMusicSetup),
            Key::Enter => Some(UiAction::SubmitYandexMusicSetup),
            Key::F(1) => Some(UiAction::OpenYandexOAuthGuide),
            Key::Backspace => Some(UiAction::DeleteYandexMusicTokenCharacter),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeleteYandexMusicTokenWord)
            }
            Key::Char(character) if !character.is_control() && !key.chorded() => {
                Some(UiAction::AppendYandexMusicTokenCharacter(character))
            }
            _ => None,
        };
    }
    if let Some(setup) = view.youtube_setup_popup.as_ref() {
        let other_field = match setup.selected_field {
            YouTubeSetupField::ApiKey => YouTubeSetupField::InvidiousUrl,
            YouTubeSetupField::InvidiousUrl => YouTubeSetupField::ApiKey,
        };
        return match key.key {
            Key::Esc => Some(UiAction::DismissYouTubeSetup),
            Key::Enter => Some(UiAction::SubmitYouTubeSetup),
            Key::F(1) => Some(UiAction::OpenYouTubeApiKeyGuide),
            Key::F(2) => Some(UiAction::OpenGoogleCloudCredentials),
            Key::F(3) => Some(UiAction::OpenInvidiousInstances),
            Key::Tab | Key::BackTab | Key::Up | Key::Down => {
                Some(UiAction::SelectYouTubeSetupField(other_field))
            }
            Key::Backspace => Some(UiAction::DeleteYouTubeSetupCharacter),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeleteYouTubeSetupWord)
            }
            Key::Char(character) if !character.is_control() && !key.chorded() => {
                Some(UiAction::AppendYouTubeSetupCharacter(character))
            }
            _ => None,
        };
    }
    if key.ctrl && key.key == Key::Char('c') {
        return Some(UiAction::Quit);
    }
    if view.help_open {
        return match key.key {
            Key::Char('?') | Key::Esc => Some(UiAction::ToggleHelp),
            Key::F(9) => Some(UiAction::OpenProjectHistory),
            Key::Char('q') => Some(UiAction::Quit),
            _ => None,
        };
    }
    let thumbnail_expanded = view
        .details
        .as_ref()
        .is_some_and(|details| details.thumbnail_expanded);
    if thumbnail_expanded && key.key == Key::Esc {
        return Some(UiAction::ToggleThumbnailExpansion);
    }
    if view.expanded_thumbnail_available() {
        return None;
    }
    if view.search_editing {
        return match key.key {
            Key::Esc => Some(UiAction::CancelSearch),
            Key::Enter => Some(UiAction::SubmitSearch),
            Key::Backspace => Some(UiAction::DeleteSearchCharacter),
            Key::Left => Some(UiAction::MoveSearchCursor(-1)),
            Key::Right => Some(UiAction::MoveSearchCursor(1)),
            Key::Char('w' | 'W') if is_delete_previous_word_key(key) => {
                Some(UiAction::DeleteSearchWord)
            }
            Key::Char(character) if !key.chorded() => Some(UiAction::AppendSearch(character)),
            _ => None,
        };
    }
    if is_delete_previous_word_key(key) {
        return None;
    }

    let alt = key.alt;
    let detail_link_count = view
        .details
        .as_ref()
        .map_or(0, |details| details.links.len());
    let wikidata_media_count = view
        .details
        .as_ref()
        .and_then(|details| {
            let item_id = details.expanded_wikidata_item.as_deref()?;
            details
                .wikidata_entities
                .iter()
                .find(|entity| entity.item_id == item_id)
        })
        .map_or(0, |entity| entity.media_controls.len());
    let details_line_scroll_available = details_accept_line_scroll(view);
    let wikidata_link_index = keyboard_wikidata_link_index(view);
    match key.key {
        Key::Char('q') => Some(UiAction::Quit),
        #[cfg(feature = "qr")]
        Key::Char('Q')
            if !key.chorded()
                && view.details.as_ref().is_some_and(|details| {
                    details
                        .media_id
                        .as_ref()
                        .is_some_and(|media_id| media_id.source == SourceKind::YouTube)
                }) =>
        {
            Some(UiAction::OpenVideoQr)
        }
        Key::Char('?') => Some(UiAction::ToggleHelp),
        Key::F(9) => Some(UiAction::OpenProjectHistory),
        Key::Char('/') => Some(UiAction::BeginSearch),
        Key::Char('p') | Key::F(7) => Some(UiAction::OpenPreferences),
        Key::Tab if reverse_tab(key) => Some(UiAction::ShowScreen(
            view.screen
                .previous_available(view.playback_history_enabled),
        )),
        Key::Tab => Some(UiAction::ShowScreen(
            view.screen.next_available(view.playback_history_enabled),
        )),
        Key::BackTab => Some(UiAction::ShowScreen(
            view.screen
                .previous_available(view.playback_history_enabled),
        )),
        Key::Char('S') => Some(UiAction::ShowScreen(Screen::Subscriptions)),
        Key::F(2) => Some(UiAction::ShowScreen(Screen::Downloaded)),
        Key::F(3) if view.playback_history_enabled => Some(UiAction::ShowScreen(Screen::History)),
        Key::F(4) => Some(UiAction::ShowScreen(Screen::Playlists)),
        Key::F(5) => Some(UiAction::ShowScreen(Screen::Statistics)),
        Key::Char('v') if view.screen == Screen::YandexMusic => {
            Some(UiAction::CycleYandexMusicSearchKind)
        }
        Key::Char('v') => Some(UiAction::ToggleSearchKind),
        Key::Char('N') => Some(UiAction::ToggleYouTubeSearchSort),
        Key::Char('n') if key.ctrl => Some(UiAction::PlayNext),
        Key::Char('C') if view.screen != Screen::YandexMusic => {
            Some(UiAction::ToggleYouTubeCreativeCommons)
        }
        Key::Char('A') => Some(UiAction::ToggleAutoplay),
        #[cfg(feature = "commons-upload")]
        Key::Char('U') if !key.chorded() && view.commons_upload_available => {
            Some(UiAction::OpenCommonsUpload)
        }
        #[cfg(feature = "evernote")]
        Key::Char('E') if !key.chorded() && view.evernote_available => {
            Some(UiAction::OpenEvernoteNote)
        }
        Key::Char('l') if view.playlist_item.is_some() && !key.modified() => {
            Some(UiAction::ToggleTodoPlaylist)
        }
        Key::Char('P') if view.playlist_item.is_some() && !key.chorded() => {
            Some(UiAction::OpenPlaylistPopup)
        }
        Key::Char('e') if view.screen == Screen::Playlists && view.playlist_edit_available => {
            Some(UiAction::EditSelectedPlaylist)
        }
        Key::Char('Z') if view.screen == Screen::Local && view.local_folder_sizes_enabled => {
            Some(UiAction::ToggleLocalSizeSort)
        }
        Key::Char('H') if view.screen == Screen::Local => Some(UiAction::ToggleLocalAllFiles),
        Key::Char('B') if view.screen == Screen::Radio => Some(UiAction::CycleRadioSort),
        Key::Char('f') if view.screen == Screen::Radio => Some(UiAction::ToggleRadioFavorite),
        Key::Char('L')
            if view.screen == Screen::YandexMusic && view.yandex_music_actions.track_selected =>
        {
            Some(UiAction::ToggleYandexMusicLike)
        }
        Key::Char('X')
            if view.screen == Screen::YandexMusic && view.yandex_music_actions.track_selected =>
        {
            Some(UiAction::ToggleYandexMusicDislike)
        }
        Key::Char('g')
            if view.screen == Screen::YandexMusic && view.yandex_music_actions.artist_available =>
        {
            Some(UiAction::OpenYandexMusicArtist)
        }
        Key::Char('b')
            if view.screen == Screen::YandexMusic && view.yandex_music_actions.album_available =>
        {
            Some(UiAction::OpenYandexMusicAlbum)
        }
        Key::Char('D')
            if view.screen == Screen::YandexMusic && view.yandex_music_actions.album_open =>
        {
            Some(UiAction::DownloadYandexMusicAlbum)
        }
        Key::Char('R')
            if view.screen == Screen::YandexMusic
                && view.yandex_music_actions.twenty_recommendations_available =>
        {
            Some(UiAction::DownloadTwentyYandexMusicRecommendations)
        }
        Key::Char('f')
            if view.screen == Screen::Local
                && view
                    .details
                    .as_ref()
                    .is_some_and(|details| details.local_fingerprint_available) =>
        {
            Some(UiAction::FingerprintLocalAudio)
        }
        Key::Char('V')
            if !key.chorded()
                && view.screen == Screen::Local
                && view.audio_quality_supported
                && view
                    .details
                    .as_ref()
                    .is_some_and(|details| details.local_audio_quality_available) =>
        {
            Some(UiAction::AnalyzeLocalAudioQuality)
        }
        Key::Char('r') if view.screen == Screen::Radio => Some(UiAction::ToggleRadioRecording),
        Key::Char('T') => Some(UiAction::ToggleChapterTimestamps),
        #[cfg(feature = "local-rename")]
        Key::Char('r') if view.screen == Screen::Local => Some(UiAction::BeginLocalRename),
        #[cfg(feature = "local-move")]
        Key::Char('m') if view.screen == Screen::Local && !key.modified() => {
            Some(UiAction::BeginLocalMove)
        }
        #[cfg(any(feature = "local-move", feature = "audio-quality"))]
        Key::Char('J') if view.screen == Screen::Local && key.shift => {
            Some(UiAction::ExtendLocalMoveSelection(1))
        }
        #[cfg(any(feature = "local-move", feature = "audio-quality"))]
        Key::Char('K') if view.screen == Screen::Local && key.shift => {
            Some(UiAction::ExtendLocalMoveSelection(-1))
        }
        #[cfg(any(feature = "local-move", feature = "audio-quality"))]
        Key::Char('j') if view.screen == Screen::Local && key.shift => {
            Some(UiAction::ExtendLocalMoveSelection(1))
        }
        #[cfg(any(feature = "local-move", feature = "audio-quality"))]
        Key::Char('k') if view.screen == Screen::Local && key.shift => {
            Some(UiAction::ExtendLocalMoveSelection(-1))
        }
        #[cfg(feature = "local-trash")]
        Key::Delete if view.screen == Screen::Local => Some(UiAction::RequestLocalTrash),
        #[cfg(feature = "local-trash")]
        Key::Char('x') if view.screen == Screen::Downloaded && !key.modified() => {
            Some(UiAction::RequestDownloadedTrash)
        }
        Key::Char('i')
            if view.screen == Screen::Subscriptions
                && view.subscriptions.layout == SubscriptionsLayout::Split
                && !view.subscriptions.items.is_empty() =>
        {
            Some(UiAction::ToggleSubscriptionDescription)
        }
        Key::Char('a')
            if view.screen == Screen::Subscriptions
                && view.subscriptions.route == SubscriptionRoute::Sources
                && !key.chorded() =>
        {
            Some(UiAction::OpenRssSubscriptionPopup)
        }
        Key::Char('W') => wikidata_link_index.map(UiAction::ToggleWikidataStatements),
        Key::Char('h')
            if !key.modified()
                && subscription_items_active(view)
                && view.subscriptions.source_kind == SubscriptionKind::YouTube =>
        {
            Some(UiAction::ToggleSubscriptionShorts)
        }
        Key::Char('R') if subscription_items_active(view) => {
            Some(UiAction::RefreshSubscriptionVideos)
        }
        Key::Char('t')
            if view.details.is_some() && view.right_panel_mode == RightPanelMode::Details =>
        {
            Some(UiAction::ToggleTextSelectionMode)
        }
        Key::Char('j') if alt && view.details_focused && wikidata_media_count > 0 => {
            Some(UiAction::MoveWikidataMedia(1))
        }
        Key::Char('k') if alt && view.details_focused && wikidata_media_count > 0 => {
            Some(UiAction::MoveWikidataMedia(-1))
        }
        Key::Char('j') if alt && detail_link_count > 0 => Some(UiAction::MoveDetailLink(1)),
        Key::Char('k') if alt && detail_link_count > 0 => Some(UiAction::MoveDetailLink(-1)),
        Key::Home if alt && detail_link_count > 0 => Some(UiAction::SelectDetailLink(0)),
        Key::End if alt && detail_link_count > 0 => {
            Some(UiAction::SelectDetailLink(detail_link_count - 1))
        }
        Key::Esc
            if view.screen == Screen::Subscriptions
                && (view.subscriptions.description_expanded
                    || view.subscriptions.focus == SubscriptionPane::Items) =>
        {
            Some(UiAction::GoBack)
        }
        Key::Esc if view.details_focused => Some(UiAction::SetDetailsFocus(false)),
        Key::Esc
            if view.screen == Screen::YandexMusic
                && matches!(
                    view.yandex_music_route,
                    YandexMusicRouteView::Search
                        | YandexMusicRouteView::Album
                        | YandexMusicRouteView::Artist
                ) =>
        {
            Some(UiAction::GoBack)
        }
        Key::Esc if view.playlist_back_available => Some(UiAction::GoBack),
        Key::Esc if view.screen == Screen::LibriVox => Some(UiAction::GoBack),
        Key::Esc if view.screen == Screen::Local => Some(UiAction::OpenLocalParent),
        Key::Up if alt && details_line_scroll_available => {
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(-1)))
        }
        Key::Down if alt && details_line_scroll_available => {
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(1)))
        }
        Key::Char('u') if alt && details_line_scroll_available => {
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(-1)))
        }
        Key::Char('d') if alt && details_line_scroll_available => {
            Some(UiAction::ScrollDetails(DetailsScroll::Lines(1)))
        }
        Key::Up | Key::Down if alt => None,
        Key::Char('u' | 'd') if alt => None,
        Key::PageUp if view.details_focused => {
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(-1)))
        }
        Key::PageDown if view.details_focused => {
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(1)))
        }
        Key::PageUp
            if matches!(
                view.screen,
                Screen::LibriVox | Screen::Local | Screen::Radio | Screen::Subscriptions
            ) =>
        {
            page_rows
                .filter(|rows| *rows > 0)
                .map(|rows| UiAction::MoveSelection(-i32::try_from(rows).unwrap_or(i32::MAX)))
        }
        Key::PageDown
            if matches!(
                view.screen,
                Screen::LibriVox | Screen::Local | Screen::Radio | Screen::Subscriptions
            ) =>
        {
            page_rows
                .filter(|rows| *rows > 0)
                .map(|rows| UiAction::MoveSelection(i32::try_from(rows).unwrap_or(i32::MAX)))
        }
        Key::Home if view.details_focused => Some(UiAction::ScrollDetails(DetailsScroll::Home)),
        Key::End if view.details_focused => Some(UiAction::ScrollDetails(DetailsScroll::End)),
        Key::Char('j') => Some(UiAction::MoveSelection(1)),
        Key::Char('k') => Some(UiAction::MoveSelection(-1)),
        Key::F(6)
            if view.video_comments_available
                && view.details.as_ref().is_some_and(|details| {
                    details
                        .media_id
                        .as_ref()
                        .is_some_and(|media_id| media_id.source == SourceKind::YouTube)
                }) =>
        {
            Some(UiAction::OpenVideoComments)
        }
        Key::Char('G') if !key.chorded() && view.video_summary_available => {
            Some(UiAction::GenerateVideoSummary)
        }
        Key::Enter if alt && detail_link_count > 0 => {
            let selected = view
                .selected_detail_link
                .unwrap_or_default()
                .min(detail_link_count - 1);
            Some(UiAction::ActivateDetailLink(selected))
        }
        Key::Enter if view.details_focused && wikidata_media_count > 0 => {
            Some(UiAction::ActivateWikidataMedia(
                view.selected_wikidata_media
                    .unwrap_or_default()
                    .min(wikidata_media_count - 1),
            ))
        }
        Key::Enter => Some(UiAction::ActivateSelection),
        Key::Char(' ') => Some(UiAction::TogglePause),
        Key::Left if alt => Some(UiAction::GoBack),
        Key::Right if alt => Some(UiAction::GoForward),
        Key::Left if view.playback.seeking_available() => Some(UiAction::SeekRelative(-5)),
        Key::Right if view.playback.seeking_available() => Some(UiAction::SeekRelative(5)),
        Key::Up => Some(UiAction::ChangeVolume(5)),
        Key::Down => Some(UiAction::ChangeVolume(-5)),
        Key::Char('<') | Key::Char(',') => Some(UiAction::ChangeSpeed(-0.1)),
        Key::Char('>') | Key::Char('.') => Some(UiAction::ChangeSpeed(0.1)),
        Key::Char('[')
            if !key.modified() && view.playback.seeking_available() && !view.playback.live =>
        {
            Some(UiAction::ChangeChapter(-1))
        }
        Key::Char(']')
            if !key.modified() && view.playback.seeking_available() && !view.playback.live =>
        {
            Some(UiAction::ChangeChapter(1))
        }
        // Shifted neighbours of the chapter keys, for the next size up: `[`/`]`
        // move within one item, `{`/`}` move between them. Unlike the chapter
        // keys these carry no condition, because the queue is answerable even
        // when the current item is a live stream that cannot be sought.
        Key::Char('{') => Some(UiAction::PlayQueueNeighbour(-1)),
        Key::Char('}') => Some(UiAction::PlayQueueNeighbour(1)),
        Key::Char('r') => Some(UiAction::ToggleRepeat),
        Key::Char('w') if !key.modified() => Some(UiAction::ToggleWaveform),
        Key::Char('c') => Some(UiAction::ShowChannel),
        Key::Char('s') => Some(UiAction::ToggleSubscription),
        Key::Backspace => Some(UiAction::GoBack),
        Key::Char('n') if !key.modified() => Some(UiAction::EditPrivateNote),
        Key::Char('a') => Some(UiAction::AddToQueue),
        Key::Char('d') => Some(UiAction::Download),
        Key::Char('o') => Some(UiAction::OpenInBrowser),
        Key::Char('O') if view.screen == Screen::Radio => Some(UiAction::OpenInBrowser),
        Key::Char('O') => Some(UiAction::OpenChannelInBrowser),
        Key::Char('y') => Some(UiAction::CopyLink),
        Key::Char('u') if !key.modified() => Some(UiAction::OpenQueuePopup),
        Key::Char(digit @ '0'..='9') if view.playback.seeking_available() => {
            let percentage = f64::from(digit.to_digit(10).unwrap_or_default()) * 10.0;
            Some(UiAction::SeekPercent(percentage))
        }
        _ => None,
    }
}

/// Reports whether line-scrolling shortcuts can target the visible Details pane.
///
/// The default Linux virtual-console keymap binds `Alt+Up` to its
/// `KeyboardSignal` action and emits `Alt+Down` like plain `Down`, so neither
/// chord reaches Crossterm as an Alt-modified arrow. [`key_action`] retains
/// modifier-aware arrows for terminal emulators and also accepts `Alt+u/d` as
/// virtual-console-safe aliases. Both paths use this predicate so a failed
/// `Alt+d` scroll never falls through to the unrelated Download action.
fn details_accept_line_scroll(view: &ViewModel) -> bool {
    view.details.is_some() && view.right_panel_mode == RightPanelMode::Details
}

/// Resolves the Wikidata disclosure owned by the global `W` shortcut.
///
/// An expanded item takes precedence so `W` always collapses the visible
/// spoiler. Otherwise the explicitly selected Wikidata row wins, followed by
/// the first Wikidata row when asynchronous enrichment has not selected one.
fn keyboard_wikidata_link_index(view: &ViewModel) -> Option<usize> {
    let details = view.details.as_ref()?;
    let index_for_item = |item_id: &str| {
        details
            .links
            .iter()
            .position(|link| link.wikidata_item_id.as_deref() == Some(item_id))
    };
    details
        .expanded_wikidata_item
        .as_deref()
        .and_then(index_for_item)
        .or_else(|| {
            view.selected_detail_link.filter(|index| {
                details
                    .links
                    .get(*index)
                    .is_some_and(|link| link.wikidata_item_id.is_some())
            })
        })
        .or_else(|| {
            details
                .links
                .iter()
                .position(|link| link.wikidata_item_id.is_some())
        })
}

/// Recognizes reverse-tab modifier encodings produced by terminal keyboards.
///
/// Modern terminals normally report either [`Key::BackTab`] or
/// `Shift+Tab`. The default Linux virtual-console keymap emits Backtab as
/// `Escape` followed by `Tab`, which Crossterm exposes as `Alt+Tab`. That
/// encoding cannot be distinguished from a literal Alt+Tab, so both forms are
/// intentionally accepted as the previous-screen shortcut.
const fn reverse_tab(key: KeyPress) -> bool {
    key.shift || key.alt
}
