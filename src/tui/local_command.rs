//! Terminal-only command input, history selection, and foreground Bash handoff.

use std::io::Write;

use super::*;
use crate::local_command::LocalCommandPlan;

/// Surrenders the TTY to Bash and retains its output until one key is pressed.
///
/// The portable reader is stopped before changing terminal ownership. Bash's
/// fixed outer wrapper restores foreground job control even when the user's
/// command executes `exec` or is interrupted. Every completion path re-enters
/// the TUI and restarts its reader before returning a result to the controller.
pub(super) fn execute_local_command_plan(
    session: &mut TerminalSession,
    input: &mut TerminalInput,
    plan: LocalCommandPlan,
) -> Result<(), String> {
    input.suspend();
    if let Err(error) = session.suspend() {
        let _ = session.resume();
        input.resume();
        return Err(format!("cannot suspend the terminal UI: {error}"));
    }
    let outcome = show_command_output(&plan);
    let resumed = session.resume();
    input.resume();
    resumed.map_err(|error| format!("cannot restore the terminal UI: {error}"))?;
    outcome
}

/// Uses inherited streams: output is neither buffered in RAM nor sent to logs.
fn show_command_output(plan: &LocalCommandPlan) -> Result<(), String> {
    let result = plan
        .command()
        .and_then(|mut command| {
            command.status().map_err(|error| {
                format!("cannot run Bash (install bash and ensure it is on PATH): {error}")
            })
        })
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("Command finished with {status}"))
            }
        });
    if let Err(error) = &result {
        writeln!(io::stdout(), "\n{error}").map_err(|error| error.to_string())?;
    }
    enable_raw_mode().map_err(|error| error.to_string())?;
    write!(io::stdout(), "\r\nPress any key to return to Youta\r\n")
        .and_then(|()| io::stdout().flush())
        .map_err(|error| error.to_string())?;
    loop {
        if matches!(
            crossterm::event::read().map_err(|error| error.to_string())?,
            Event::Key(KeyEvent {
                kind: KeyEventKind::Press,
                ..
            })
        ) {
            return result;
        }
    }
}

/// Reserves a clear, black-on-light editor in the existing seek-bar area.
pub(super) fn render_command(
    frame: &mut Frame<'_>,
    area: Rect,
    editor: &LocalCommandView,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);
    if area.is_empty() {
        return;
    }
    let row = Rect::new(area.x, area.y, area.width, 1);
    let style = Style::default().fg(Color::Black).bg(Color::Gray);
    frame.render_widget(Paragraph::new(":").style(style), row);
    let field = command_field(area, 1);
    let viewport = rename_field_viewport(&editor.command, editor.cursor_byte, field.width);
    frame.render_widget(
        Paragraph::new(&editor.command[viewport.start_byte..]).style(style),
        field,
    );
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(
                "Enter run  Esc cancel  Tab complete  Up/Down history  Ctrl+R search  % selected path",
            )
            .style(theme.muted),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
}

/// Reserves exactly the visible prompt width while horizontally scrolling its input.
fn command_field(area: Rect, prefix_width: u16) -> Rect {
    let prefix = area.width.min(prefix_width);
    Rect::new(
        area.x + prefix,
        area.y,
        area.width - prefix,
        u16::from(area.height > 0),
    )
}

/// Fits up to ten history matches plus the query and keyboard instructions.
fn history_area(area: Rect, matches: usize) -> Rect {
    let width = area.width.saturating_sub(4).max(area.width.min(4));
    let height = u16::try_from(matches.min(10))
        .unwrap_or(10)
        .max(1)
        .saturating_add(5)
        .min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Highlights every literal substring match without altering command text.
fn history_spans<'a>(command: &'a str, query: &str, style: Style) -> Vec<Span<'a>> {
    if query.is_empty() {
        return vec![Span::styled(command, style)];
    }
    let mut spans = Vec::new();
    let mut offset = 0;
    for (start, matched) in command.match_indices(query) {
        spans.push(Span::styled(&command[offset..start], style));
        spans.push(Span::styled(
            matched,
            style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
        offset = start + matched.len();
    }
    spans.push(Span::styled(&command[offset..], style));
    spans
}

/// Draws only the bounded matches supplied by the private command controller.
pub(super) fn render_history(frame: &mut Frame<'_>, view: &ViewModel, theme: &Theme) {
    let Some(history) = view
        .local_command
        .as_ref()
        .and_then(|editor| editor.history.as_ref())
    else {
        return;
    };
    let area = history_area(frame.area(), history.matches.len());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Command history ")
        .style(theme.base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let query_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(Paragraph::new("/ ").style(theme.accent), query_area);
    let field = command_field(query_area, 2);
    let viewport = rename_field_viewport(&history.query, history.cursor_byte, field.width);
    frame.render_widget(
        Paragraph::new(&history.query[viewport.start_byte..]).style(theme.base),
        field,
    );
    let capacity = usize::from(inner.height.saturating_sub(3));
    let first = history
        .selected
        .saturating_add(1)
        .saturating_sub(capacity.max(1));
    if history.matches.is_empty() && inner.height > 2 {
        frame.render_widget(
            Paragraph::new("No matching commands").style(theme.muted),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
    }
    for (index, command) in history
        .matches
        .iter()
        .enumerate()
        .skip(first)
        .take(capacity)
    {
        let style = if index == history.selected {
            theme.selected
        } else {
            theme.base
        };
        let row = Rect::new(
            inner.x,
            inner.y + 2 + (index - first) as u16,
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(Line::from(history_spans(command, &history.query, style))).style(style),
            row,
        );
    }
    if inner.height > 1 {
        frame.render_widget(
            Paragraph::new("Up/Down select  Enter run  Esc close").style(theme.muted),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

/// Places the native cursor in the visible command or history-query viewport.
pub(super) fn render_cursor(frame: &mut Frame<'_>, view: &ViewModel, enabled: bool) {
    if !enabled || view.error_popup.is_some() || view.bug_report_popup.is_some() {
        return;
    }
    let Some(editor) = view.local_command.as_ref() else {
        return;
    };
    let (area, value, cursor) = if let Some(history) = &editor.history {
        let area =
            history_area(frame.area(), history.matches.len()).inner(ratatui::layout::Margin {
                horizontal: 1,
                vertical: 1,
            });
        (
            command_field(area, 2),
            history.query.as_str(),
            history.cursor_byte,
        )
    } else {
        (
            command_field(main_frame_sections(frame.area(), view)[3], 1),
            editor.command.as_str(),
            editor.cursor_byte,
        )
    };
    if area.is_empty() {
        return;
    }
    let viewport = rename_field_viewport(value, cursor, area.width);
    frame.set_cursor_position((area.x + viewport.cursor_column, area.y));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn command_entry_uses_black_text_and_disables_seek_targets() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let view = ViewModel {
            screen: Screen::Local,
            local_command: Some(LocalCommandView {
                command: "ffmpeg -i %".to_owned(),
                cursor_byte: 11,
                history: None,
            }),
            ..ViewModel::default()
        };
        let mut hits = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hits))
            .unwrap();
        let area = main_frame_sections(Rect::new(0, 0, 80, 24), &view)[3];
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, area.y)].symbol(), ":");
        assert_eq!(buffer[(1, area.y)].symbol(), "f");
        assert_eq!(buffer[(1, area.y)].fg, Color::Black);
        assert_eq!(buffer[(1, area.y)].bg, Color::Gray);
        assert_eq!(terminal.get_cursor_position().unwrap(), (12, area.y).into());
        assert!(hits.seek_bar.is_empty());
        assert!(hits.now_playing.is_none());
        assert!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 4,
                    row: area.y,
                    modifiers: KeyModifiers::NONE
                },
                &hits,
                &view
            )
            .is_none()
        );
    }

    /// Empty prompts start immediately after ':' while history retains its '/ ' prefix.
    #[test]
    fn command_cursor_has_no_padding_but_history_keeps_its_spacing() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut view = ViewModel {
            screen: Screen::Local,
            local_command: Some(LocalCommandView::default()),
            ..ViewModel::default()
        };
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut HitMap::default()))
            .unwrap();
        let area = main_frame_sections(Rect::new(0, 0, 80, 24), &view)[3];
        assert_eq!(terminal.get_cursor_position().unwrap(), (1, area.y).into());

        view.local_command.as_mut().unwrap().history = Some(LocalCommandHistoryView {
            query: "echo".to_owned(),
            cursor_byte: 4,
            ..LocalCommandHistoryView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut HitMap::default()))
            .unwrap();
        let popup = history_area(Rect::new(0, 0, 80, 24), 0);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(popup.x + 1, popup.y + 1)].symbol(), "/");
        assert_eq!(buffer[(popup.x + 2, popup.y + 1)].symbol(), " ");
        assert_eq!(buffer[(popup.x + 3, popup.y + 1)].symbol(), "e");
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            (popup.x + 7, popup.y + 1).into()
        );
    }

    /// A diagnostic composer above the editor retains its own clickable controls.
    #[test]
    fn bug_report_overlay_accepts_clicks_without_activating_command_background() {
        let mut view = ViewModel {
            local_command: Some(LocalCommandView::default()),
            ..ViewModel::default()
        };
        let hits = HitMap {
            bug_report_buttons: vec![(UiAction::DismissBugReport, Rect::new(4, 5, 6, 1))],
            ..HitMap::default()
        };
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        assert!(mouse_action(click, &hits, &view).is_none());
        view.bug_report_popup = Some(BugReportPopupView::default());
        assert!(matches!(
            mouse_action(click, &hits, &view),
            Some(UiAction::DismissBugReport)
        ));
    }

    #[test]
    fn history_highlights_substrings_and_narrow_inputs_preserve_unicode() {
        let command = "echo минск; echo минск";
        let spans = history_spans(command, "минск", Style::default());
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
                .count(),
            2
        );
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            command
        );
        for width in [1, 2, 3, 8, 30, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            let view = ViewModel {
                local_command: Some(LocalCommandView {
                    command: "echo cafe\u{301}東京".to_owned(),
                    cursor_byte: usize::MAX,
                    history: Some(LocalCommandHistoryView {
                        query: "минск".to_owned(),
                        cursor_byte: usize::MAX,
                        matches: vec![command.to_owned(); 10],
                        selected: 9,
                    }),
                }),
                ..ViewModel::default()
            };
            terminal
                .draw(|frame| render(frame, &view, &UiSettings::default(), &mut HitMap::default()))
                .unwrap();
        }
    }
}
