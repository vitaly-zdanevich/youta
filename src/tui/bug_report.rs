//! Plain-text screen capture and the terminal's manual bug-report composer.

use super::*;

/// Extracts bounded, text-only cells without terminal-image protocol payloads.
pub(super) fn capture_screen(buffer: &ratatui::buffer::Buffer, image_area: Option<Rect>) -> String {
    const TRUNCATED: &str = "\n[Screenshot truncated]";
    let budget = crate::bug_report::MAX_SCREENSHOT_BYTES.saturating_sub(TRUNCATED.len());
    let mut text = String::new();
    let mut truncated = false;
    'rows: for y in buffer.area.top()..buffer.area.bottom() {
        let mut row = String::new();
        let mut spaces = 0_usize;
        let mut covered_columns = 0_u16;
        for x in buffer.area.left()..buffer.area.right() {
            if covered_columns > 0 {
                covered_columns -= 1;
                continue;
            }
            let symbol = buffer[(x, y)].symbol();
            let masked = image_area.is_some_and(|area| contains(area, x, y))
                || symbol.len() > budget
                || symbol
                    .chars()
                    .any(|character| character.is_control() || character == '\u{10eeee}')
                || symbol.graphemes(true).take(2).count() != 1;
            if masked || symbol == " " || symbol.is_empty() {
                spaces = spaces.saturating_add(1);
                continue;
            }
            if text
                .len()
                .saturating_add(row.len())
                .saturating_add(spaces)
                .saturating_add(symbol.len())
                .saturating_add(1)
                > budget
            {
                text.push_str(&row);
                truncated = true;
                break 'rows;
            }
            row.extend(std::iter::repeat_n(' ', spaces));
            spaces = 0;
            row.push_str(symbol);
            covered_columns = terminal_text_width(symbol).saturating_sub(1);
        }
        if text.len().saturating_add(row.len()).saturating_add(1) > budget {
            truncated = true;
            break;
        }
        text.push_str(&row);
        text.push('\n');
    }
    while text.ends_with('\n') {
        text.pop();
    }
    if truncated {
        text.push_str(TRUNCATED);
    }
    crate::bug_report::sanitize_screenshot(&text)
}

/// Supplies the last completed frame before opening the report composer.
pub(super) fn dispatch_terminal_action(
    controller: &mut impl UiController,
    action: UiAction,
    buffer: &ratatui::buffer::Buffer,
    hit_map: &HitMap,
) {
    if matches!(action, UiAction::OpenBugReport) && controller.view().bug_report_popup.is_none() {
        let screenshot = controller
            .view()
            .bug_report_screenshot_allowed()
            .then(|| capture_screen(buffer, hit_map.thumbnail_area));
        controller.open_bug_report(screenshot);
    } else {
        controller.dispatch(action);
    }
}

/// Renders the editable form above ordinary modals without an extra review step.
pub(super) fn render_popup(
    frame: &mut Frame<'_>,
    popup: &BugReportPopupView,
    show_hotkeys: bool,
    external_opener_available: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_sized_rect(100, 28, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Bug report ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let editable = matches!(
        popup.submission,
        GitHubIssueSubmissionView::Idle | GitHubIssueSubmissionView::Failed { .. }
    );
    let pending = matches!(popup.submission, GitHubIssueSubmissionView::Submitting);
    let sections = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(if popup.screenshot_notice.is_some() {
            2
        } else {
            0
        }),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(if inner.width < 70 { 2 } else { 1 }),
    ])
    .split(inner);
    for (field, label, value, cursor, field_area) in [
        (
            BugReportField::Title,
            "Title",
            popup.title.as_str(),
            popup.title_cursor_byte,
            sections[0],
        ),
        (
            BugReportField::Body,
            "Body",
            popup.body.as_str(),
            popup.body_cursor_byte,
            sections[1],
        ),
    ] {
        let selected = editable && popup.selected_field == field;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(if selected { theme.accent } else { theme.border })
            .title(format!(" {label} "));
        let text_area = block.inner(field_area);
        frame.render_widget(block, field_area);
        if editable && !field_area.is_empty() {
            hit_map.bug_report_fields.push((field, field_area));
        }
        if text_area.is_empty() {
            continue;
        }
        if field == BugReportField::Title {
            let mut displayed = value.to_owned();
            if selected {
                displayed.insert_str(editor_cursor_boundary(value, cursor), "▏");
            }
            let viewport = rename_field_viewport(&displayed, cursor, text_area.width);
            frame.render_widget(
                Paragraph::new(&displayed[viewport.start_byte..]).style(theme.base),
                text_area,
            );
        } else {
            let wrapped = wrap_editor_text(value, cursor, text_area.width, selected);
            let visible = usize::from(text_area.height);
            let maximum = wrapped.lines.len().saturating_sub(visible);
            let mut offset = popup.body_scroll_offset.min(maximum);
            if popup.follow_cursor {
                if wrapped.cursor_row < offset {
                    offset = wrapped.cursor_row;
                } else if wrapped.cursor_row >= offset.saturating_add(visible) {
                    offset = wrapped.cursor_row.saturating_add(1).saturating_sub(visible);
                }
            }
            let lines = wrapped
                .lines
                .into_iter()
                .skip(offset)
                .take(visible)
                .map(Line::raw)
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines).style(theme.base), text_area);
        }
    }
    let screenshot_enabled = editable && popup.screenshot_available;
    let screenshot_style = if !screenshot_enabled {
        theme.muted
    } else if popup.selected_field == BugReportField::Screenshot {
        theme.selected
    } else {
        theme.base
    };
    let screenshot_label = format!(
        "[{}] With ASCII screenshot",
        if popup.with_screenshot { 'x' } else { ' ' }
    );
    frame.render_widget(
        Paragraph::new(screenshot_label.as_str()).style(screenshot_style),
        sections[2],
    );
    if screenshot_enabled && !sections[2].is_empty() {
        hit_map.bug_report_buttons.push((
            UiAction::ToggleBugReportScreenshot,
            Rect::new(
                sections[2].x,
                sections[2].y,
                terminal_text_width(&screenshot_label).min(sections[2].width),
                1,
            ),
        ));
    }
    if let Some(notice) = &popup.screenshot_notice {
        frame.render_widget(
            Paragraph::new(notice.as_str())
                .style(theme.muted)
                .wrap(Wrap { trim: false }),
            sections[3],
        );
    }
    frame.render_widget(
        Paragraph::new(popup.footer.as_str())
            .style(theme.muted)
            .wrap(Wrap { trim: false }),
        sections[4],
    );
    let status = status_text(popup);
    frame.render_widget(
        Paragraph::new(status)
            .style(if popup.validation_error.is_some() {
                Style::default().fg(Color::Red)
            } else {
                theme.base
            })
            .wrap(Wrap { trim: false }),
        sections[5],
    );
    let mut buttons = vec![
        (
            button("Ctrl+S", "Submit", show_hotkeys),
            UiAction::SubmitBugReport,
            editable && popup.gh_available,
        ),
        ("Copy".to_owned(), UiAction::CopyBugReport, !pending),
    ];
    match popup.submission {
        GitHubIssueSubmissionView::Submitted { .. } => buttons.push((
            "Open issue".to_owned(),
            UiAction::OpenBugReportResult,
            external_opener_available,
        )),
        GitHubIssueSubmissionView::OutcomeUnknown { .. } => buttons.push((
            "Check issues".to_owned(),
            UiAction::OpenBugReportResult,
            external_opener_available,
        )),
        _ => {}
    }
    buttons.push((
        button(
            "Esc",
            if editable { "Cancel" } else { "Close" },
            show_hotkeys,
        ),
        UiAction::DismissBugReport,
        !pending,
    ));
    render_buttons(
        frame,
        sections[6],
        &buttons,
        theme,
        &mut hit_map.bug_report_buttons,
    );
}

/// Keeps submission progress and its bounded result inside the original form.
fn status_text(popup: &BugReportPopupView) -> String {
    let notice = match &popup.submission {
        GitHubIssueSubmissionView::Submitting => format!(
            "{} Submitting to GitHub...",
            ASCII_ACTIVITY_FRAMES[popup.animation_frame % ASCII_ACTIVITY_FRAMES.len()]
        ),
        GitHubIssueSubmissionView::Submitted { url } => format!("GitHub issue created:\n{url}"),
        GitHubIssueSubmissionView::OutcomeUnknown { issues_url } => {
            format!(
                "GitHub may have created the issue. Check existing issues before retrying.\n{issues_url}"
            )
        }
        GitHubIssueSubmissionView::Failed { message } => format!("Submission failed: {message}"),
        _ if !popup.gh_available => {
            "GitHub CLI (gh) is unavailable. Copy the report to file an issue manually.".to_owned()
        }
        _ => "Reports are public. Tab changes fields; Ctrl+S or Ctrl+Enter submits; Esc cancels."
            .to_owned(),
    };
    match (&popup.submission, &popup.validation_error) {
        (
            GitHubIssueSubmissionView::Submitted { .. }
            | GitHubIssueSubmissionView::OutcomeUnknown { .. },
            Some(error),
        ) => format!("{notice}\n{error}"),
        (GitHubIssueSubmissionView::Submitting, _) | (_, None) => notice,
        (_, Some(error)) => error.clone(),
    }
}

/// Wraps enabled and disabled controls together while registering only visible hits.
fn render_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    buttons: &[(String, UiAction, bool)],
    theme: &Theme,
    targets: &mut Vec<(UiAction, Rect)>,
) {
    if area.is_empty() {
        return;
    }
    let mut x = area.x;
    let mut y = area.y;
    for (label, action, enabled) in buttons {
        let width = terminal_text_width(label).min(area.width);
        if x > area.x && x.saturating_add(width) > area.right() {
            x = area.x;
            y = y.saturating_add(1);
        }
        if y >= area.bottom() {
            break;
        }
        let target = Rect::new(x, y, width, 1);
        frame.render_widget(
            Paragraph::new(truncate_terminal_text(label, usize::from(width))).style(if *enabled {
                theme.accent
            } else {
                theme.muted
            }),
            target,
        );
        if *enabled && !target.is_empty() {
            targets.push((action.clone(), target));
        }
        x = x.saturating_add(width).saturating_add(3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// A covered QR must not hide the pointer used to operate the report form.
    #[cfg(feature = "qr")]
    #[test]
    fn bug_report_above_qr_keeps_the_virtual_pointer_visible() {
        let view = ViewModel {
            video_qr_popup: Some(VideoQrPopupView {
                video_id: "dQw4w9WgXcQ".to_owned(),
                video_title: "Fixture".to_owned(),
                url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned(),
                matrix: QrMatrix::encode("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap(),
            }),
            bug_report_popup: Some(BugReportPopupView::default()),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        let mut cursor = VirtualCursor {
            active: true,
            column: 50,
            row: 10,
            ..VirtualCursor::default()
        };
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut HitMap::default(),
                    None,
                );
                render_virtual_cursor_overlay(frame, &view, &mut cursor);
            })
            .unwrap();
        assert!(
            terminal.backend().buffer()[(50, 10)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    /// Copy feedback and helper errors never conceal result URLs or duplicate warnings.
    #[test]
    fn bug_report_result_guidance_survives_validation_messages_without_an_opener() {
        for (submission, url, notice) in [
            (
                GitHubIssueSubmissionView::Submitted {
                    url: "https://github.com/vitaly-zdanevich/youta/issues/123".to_owned(),
                },
                "https://github.com/vitaly-zdanevich/youta/issues/123",
                "GitHub issue created",
            ),
            (
                GitHubIssueSubmissionView::OutcomeUnknown {
                    issues_url: "https://github.com/vitaly-zdanevich/youta/issues".to_owned(),
                },
                "https://github.com/vitaly-zdanevich/youta/issues",
                "Check existing issues before retrying",
            ),
        ] {
            let view = ViewModel {
                physical_linux_console: true,
                external_opener_available: false,
                bug_report_popup: Some(BugReportPopupView {
                    submission,
                    validation_error: Some("Fixture helper feedback".to_owned()),
                    ..BugReportPopupView::default()
                }),
                ..ViewModel::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
            terminal
                .draw(|frame| {
                    render_frame(
                        frame,
                        &view,
                        &UiSettings::default(),
                        &mut HitMap::default(),
                        None,
                    )
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            for expected in [url, notice, "Fixture helper feedback"] {
                assert!(text.contains(expected), "missing {expected:?}");
            }
        }
    }

    /// Finds an exact ASCII label in physical cells, independent of nearby Unicode.
    fn label_position(buffer: &ratatui::buffer::Buffer, label: &str) -> (u16, u16) {
        for y in buffer.area.top()..buffer.area.bottom() {
            for x in buffer.area.left()..buffer.area.right() {
                if label.chars().enumerate().all(|(offset, character)| {
                    buffer
                        .cell((x.saturating_add(u16::try_from(offset).unwrap()), y))
                        .is_some_and(|cell| cell.symbol() == character.to_string())
                }) && buffer
                    .cell((x.saturating_add(u16::try_from(label.len()).unwrap()), y))
                    .is_none_or(|cell| !cell.symbol().chars().any(char::is_alphanumeric))
                {
                    return (x, y);
                }
            }
        }
        panic!("missing label {label:?} in {buffer:?}");
    }

    /// Builds a real mouse press without carrying modifiers from a prior event.
    fn click((column, row): (u16, u16)) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Screenshots preserve ordinary Unicode and drop complete image/control cells.
    #[test]
    fn bug_report_capture_preserves_text_without_image_protocol_payloads() {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 48, 3));
        buffer.set_string(0, 0, "Кириллица", Style::default());
        buffer.set_string(0, 1, "界 value", Style::default());
        buffer[(0, 2)].set_symbol("\u{1b}_GRAW-PAYLOAD\u{1b}\\");
        buffer[(1, 2)].set_symbol("\u{10EEEE}\u{305}");
        buffer[(2, 2)].set_symbol("\u{9d}OSC-PAYLOAD");
        buffer.set_string(4, 2, "PIX", Style::default());
        buffer.set_string(10, 2, "tail", Style::default());
        let screenshot = capture_screen(&buffer, Some(Rect::new(4, 2, 3, 1)));
        assert_eq!(
            screenshot.lines().collect::<Vec<_>>(),
            ["Кириллица", "界 value", "          tail"]
        );
        assert!(!screenshot.contains("PAYLOAD"));
        assert!(!screenshot.contains('\u{10EEEE}'));
    }

    /// Large screens carry an explicit truncation notice and stay within the core budget.
    #[test]
    fn bug_report_capture_is_bounded_with_an_explicit_truncation_notice() {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 100, 500));
        for cell in &mut buffer.content {
            cell.set_symbol("я");
        }
        let screenshot = capture_screen(&buffer, None);
        assert!(screenshot.len() <= crate::bug_report::MAX_SCREENSHOT_BYTES);
        assert!(screenshot.contains("truncated"));
        assert!(screenshot.starts_with("яяяя"));
    }

    /// The event seam captures the old screen once and never samples private editors.
    #[test]
    fn bug_report_capture_precedes_popup_and_private_editors_supply_none() {
        #[derive(Default)]
        struct Controller {
            view: ViewModel,
            snapshots: Vec<Option<String>>,
            forwarded: Vec<UiAction>,
        }
        impl UiController for Controller {
            fn view(&self) -> &ViewModel {
                &self.view
            }
            fn dispatch(&mut self, action: UiAction) {
                self.forwarded.push(action);
            }
            fn tick(&mut self) {}
            fn open_bug_report(&mut self, screenshot: Option<String>) {
                assert!(self.view.bug_report_popup.is_none());
                self.snapshots.push(screenshot);
                self.view.bug_report_popup = Some(BugReportPopupView::default());
            }
        }
        let mut controller = Controller::default();
        let mut terminal = Terminal::new(TestBackend::new(32, 4)).unwrap();
        let completed = terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("Original screen before composer"),
                    frame.area(),
                )
            })
            .unwrap();
        let hits = HitMap::default();
        dispatch_terminal_action(
            &mut controller,
            UiAction::OpenBugReport,
            completed.buffer,
            &hits,
        );
        assert_eq!(
            controller.snapshots,
            [Some("Original screen before composer".to_owned())]
        );
        dispatch_terminal_action(
            &mut controller,
            UiAction::CopyBugReport,
            completed.buffer,
            &hits,
        );
        assert_eq!(controller.snapshots.len(), 1);
        assert_eq!(controller.forwarded, [UiAction::CopyBugReport]);
        controller.view.bug_report_popup = None;
        controller.view.private_note_popup = Some(PrivateNotePopupView {
            body: "must stay private".to_owned(),
            ..PrivateNotePopupView::default()
        });
        dispatch_terminal_action(
            &mut controller,
            UiAction::OpenBugReport,
            completed.buffer,
            &hits,
        );
        assert_eq!(controller.snapshots.last(), Some(&None));
    }

    /// The form owns input above an existing error without adding a review screen.
    #[test]
    fn bug_report_form_renders_fields_and_exact_modal_actions() {
        for show_hotkeys in [false, true] {
            let view = ViewModel {
                bug_report_popup: Some(BugReportPopupView {
                    title: "Fixture report title".to_owned(),
                    body: "First body line\nКириллица remains readable".to_owned(),
                    screenshot_available: true,
                    gh_available: true,
                    footer: "Youta fixture on Linux".to_owned(),
                    ..BugReportPopupView::default()
                }),
                error_popup: Some(ErrorPopupView {
                    title: "Underlying error".to_owned(),
                    report: "Prior modal must not own clicks".to_owned(),
                    ..ErrorPopupView::default()
                }),
                ..ViewModel::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
            let mut hits = HitMap::default();
            terminal
                .draw(|frame| {
                    render_frame(
                        frame,
                        &view,
                        &UiSettings {
                            show_hotkeys,
                            ..UiSettings::default()
                        },
                        &mut hits,
                        None,
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rendered: String = buffer
                .content
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            for text in [
                "Bug report",
                "Fixture report title",
                "First body line",
                "Кириллица remains readable",
                "[x] With ASCII screenshot",
                "Youta fixture on Linux",
            ] {
                assert!(rendered.contains(text), "missing {text:?} in {rendered:?}");
            }
            for (label, action) in [
                (
                    "Title",
                    UiAction::SelectBugReportField(BugReportField::Title),
                ),
                ("Body", UiAction::SelectBugReportField(BugReportField::Body)),
                (
                    "[x] With ASCII screenshot",
                    UiAction::ToggleBugReportScreenshot,
                ),
                ("Submit", UiAction::SubmitBugReport),
                ("Copy", UiAction::CopyBugReport),
                ("Cancel", UiAction::DismissBugReport),
            ] {
                assert_eq!(
                    mouse_action(click(label_position(buffer, label)), &hits, &view),
                    Some(action),
                    "{label}"
                );
            }
            assert_eq!(
                mouse_action(click((0, 0)), &hits, &view),
                None,
                "underlying tabs stay modal"
            );
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        ..click((0, 0))
                    },
                    &hits,
                    &view
                ),
                None,
                "the old error must not receive wheel events"
            );
        }
    }

    /// Submission remains visible but gray, non-clickable, and regularly animated.
    #[test]
    fn bug_report_pending_submit_is_disabled_and_uses_ascii_animation() {
        let settings = UiSettings {
            idle_tick: Duration::from_secs(2),
            playing_tick: Duration::from_millis(80),
            ..UiSettings::default()
        };
        let mut view = ViewModel {
            bug_report_popup: Some(BugReportPopupView {
                gh_available: true,
                submission: GitHubIssueSubmissionView::Submitting,
                ..BugReportPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        let mut hits = HitMap::default();
        for (index, symbol) in ASCII_ACTIVITY_FRAMES.iter().enumerate() {
            view.bug_report_popup.as_mut().unwrap().animation_frame = index;
            terminal
                .draw(|frame| render_frame(frame, &view, &settings, &mut hits, None))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let submit = label_position(buffer, "Submit");
            assert_eq!(buffer[submit].fg, Color::DarkGray);
            assert_eq!(mouse_action(click(submit), &hits, &view), None);
            label_position(buffer, &format!("{symbol} Submitting"));
            assert_eq!(event_wait(&view, &settings), settings.playing_tick);
        }
    }

    /// Small viewports never allow clicks or wheels to reach covered app controls.
    #[test]
    fn bug_report_tiny_viewports_remain_modal_and_unavailable_screenshot_is_inert() {
        let view = ViewModel {
            bug_report_popup: Some(BugReportPopupView {
                screenshot_notice: Some("Screenshot unavailable for a private editor".to_owned()),
                ..BugReportPopupView::default()
            }),
            ..ViewModel::default()
        };
        for (width, height) in [(1, 1), (15, 6), (42, 12), (100, 28)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut hits = HitMap::default();
            terminal
                .draw(|frame| render_frame(frame, &view, &UiSettings::default(), &mut hits, None))
                .unwrap();
            for y in 0..height {
                for x in 0..width {
                    assert!(matches!(
                        mouse_action(click((x, y)), &hits, &view),
                        None | Some(
                            UiAction::SelectBugReportField(_)
                                | UiAction::CopyBugReport
                                | UiAction::DismissBugReport
                        )
                    ));
                }
            }
            if width == 100 {
                let buffer = terminal.backend().buffer();
                assert_eq!(
                    mouse_action(
                        click(label_position(buffer, "With ASCII screenshot")),
                        &hits,
                        &view
                    ),
                    None
                );
                assert_eq!(
                    mouse_action(click(label_position(buffer, "Submit")), &hits, &view),
                    None,
                    "missing gh disables submission"
                );
            }
        }
    }
}
