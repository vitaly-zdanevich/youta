//! Terminal pod setup and public pod selection using reducer-owned state.

use super::*;

/// Renders the manual pod draft or its directory child without changing controller state.
pub(super) fn render_popup(
    frame: &mut Frame<'_>,
    popup: &FunkwhaleSetupPopupView,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    hit_map.funkwhale_setup_buttons.clear();
    hit_map.funkwhale_pod_rows.clear();
    let picking = popup.picker.is_some();
    let area = centered_sized_rect(96, if picking { 21 } else { 13 }, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Funkwhale pod setup ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(if picking {
            "Up/Down chooses; Enter fills the URL; Esc closes the pod list."
        } else {
            "Type or paste a pod URL; Ctrl+U clears."
        })
        .style(theme.muted),
        sections[0],
    );
    let field_block = Block::default()
        .borders(Borders::ALL)
        .title(" Pod URL ")
        .border_style(if picking { theme.border } else { theme.accent });
    let field_area = field_block.inner(sections[1]);
    frame.render_widget(field_block, sections[1]);
    if !field_area.is_empty() {
        let mut displayed = if popup.url.is_empty() {
            "https://your-pod.example/".to_owned()
        } else {
            popup.url.clone()
        };
        if !picking {
            displayed.push('|');
        }
        let viewport = rename_field_viewport(&displayed, displayed.len(), field_area.width);
        frame.render_widget(
            Paragraph::new(&displayed[viewport.start_byte..]).style(if picking {
                theme.muted
            } else {
                theme.base
            }),
            field_area,
        );
    }
    if let Some(picker) = popup.picker.as_ref() {
        render_pods(frame, picker, sections[2], theme, hit_map);
    } else {
        frame.render_widget(
			Paragraph::new(
				"Choose a public pod or enter your own URL. Leave empty and save to remove the saved pod.",
			)
			.style(theme.base)
			.wrap(Wrap { trim: false }),
			sections[2],
		);
    }
    let mut status = vec![Line::styled(
        format!("Save to {}", popup.config_file_path),
        theme.muted,
    )];
    if let Some(error) = popup.error.as_ref() {
        status.push(Line::styled(
            error.as_str(),
            Style::default().fg(Color::Red),
        ));
    }
    frame.render_widget(Paragraph::new(status), sections[3]);
    let compact = sections[4].width < 53;
    let controls = if picking {
        vec![
            (
                button("F4", "Retry", show_hotkeys),
                UiAction::OpenFunkwhalePodPicker,
                true,
            ),
            (
                button("Esc", "Close pod list", show_hotkeys),
                UiAction::CloseFunkwhalePodPicker,
                true,
            ),
        ]
    } else {
        vec![
            (
                button(
                    "F4",
                    if compact { "Pods" } else { "Choose public pod" },
                    show_hotkeys,
                ),
                UiAction::OpenFunkwhalePodPicker,
                true,
            ),
            (
                button("Enter", "Save", show_hotkeys),
                UiAction::SaveFunkwhaleSetup,
                true,
            ),
            (
                button("Esc", "Cancel", show_hotkeys),
                UiAction::CloseFunkwhaleSetup,
                true,
            ),
        ]
    };
    render_buttons(
        frame,
        sections[4],
        &controls,
        theme,
        &mut hit_map.funkwhale_setup_buttons,
    );
}

/// Keeps the highlighted pod within a bounded list and registers only its displayed rows.
fn render_pods(
    frame: &mut Frame<'_>,
    picker: &FunkwhalePodPickerView,
    area: Rect,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let block = panel_block(" Public Funkwhale pods ", theme);
    let list = block.inner(area);
    frame.render_widget(block, area);
    if list.is_empty() {
        return;
    }
    let notice = if picker.loading {
        Some("Loading public pods...")
    } else if let Some(error) = picker.error.as_deref() {
        Some(error)
    } else if picker.pods.is_empty() {
        Some("No public pods found. Retry or enter a URL manually.")
    } else {
        None
    };
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(notice)
                .style(theme.base)
                .wrap(Wrap { trim: false }),
            list,
        );
        return;
    }
    let selected = picker.selected.min(picker.pods.len() - 1);
    let start = selected.saturating_sub(usize::from(list.height).saturating_sub(1));
    for (row, (index, pod)) in picker
        .pods
        .iter()
        .enumerate()
        .skip(start)
        .take(usize::from(list.height))
        .enumerate()
    {
        let target = Rect::new(
            list.x,
            list.y + u16::try_from(row).unwrap_or_default(),
            list.width,
            1,
        );
        let registration = match pod.open_registrations {
            Some(true) => " · registration open",
            Some(false) => " · registration closed",
            None => "",
        };
        let label = if pod.label.is_empty() || pod.label == pod.url {
            pod.url.clone()
        } else {
            format!("{} · {}", pod.label, pod.url)
        };
        let label = format!(
            "{} {label}{registration}",
            if index == selected { '>' } else { ' ' }
        );
        frame.render_widget(
            Paragraph::new(truncate_terminal_text(&label, usize::from(target.width))).style(
                if index == selected {
                    theme.selected
                } else {
                    theme.base
                },
            ),
            target,
        );
        hit_map.funkwhale_pod_rows.push((index, target));
    }
}

/// Wraps popup controls into visible rows and clips each hit to its rendered label.
fn render_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    controls: &[(String, UiAction, bool)],
    theme: &Theme,
    targets: &mut Vec<(UiAction, Rect)>,
) {
    if area.is_empty() {
        return;
    }
    let mut x = area.x;
    let mut y = area.y;
    for (label, action, enabled) in controls {
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
        x = x.saturating_add(width).saturating_add(2);
    }
}

/// Consumes only visible setup controls; directory wheel motion never reaches playback.
pub(super) fn mouse_action(mouse: MouseEvent, hit_map: &HitMap) -> Option<UiAction> {
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return hit_map
            .funkwhale_setup_buttons
            .iter()
            .find(|(_, area)| contains(*area, mouse.column, mouse.row))
            .map(|(action, _)| action.clone())
            .or_else(|| {
                hit_map
                    .funkwhale_pod_rows
                    .iter()
                    .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                    .map(|(index, _)| UiAction::SelectFunkwhalePod(*index))
            });
    }
    if hit_map
        .funkwhale_pod_rows
        .iter()
        .any(|(_, area)| contains(*area, mouse.column, mouse.row))
    {
        return match mouse.kind {
            MouseEventKind::ScrollDown => Some(UiAction::MoveFunkwhalePod(1)),
            MouseEventKind::ScrollUp => Some(UiAction::MoveFunkwhalePod(-1)),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Draws the actual popup and returns both its physical cells and exact control targets.
    fn render(
        popup: &FunkwhaleSetupPopupView,
        width: u16,
        height: u16,
    ) -> (ratatui::buffer::Buffer, HitMap) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hits = HitMap::default();
        terminal
            .draw(|frame| render_popup(frame, popup, true, &Theme::new(false), &mut hits))
            .unwrap();
        (terminal.backend().buffer().clone(), hits)
    }

    /// A stable manual pod fixture avoids networking and user configuration.
    fn setup() -> FunkwhaleSetupPopupView {
        FunkwhaleSetupPopupView {
            url: "https://music.example/".to_owned(),
            config_file_path: "/tmp/youta-test/config.toml".to_owned(),
            ..FunkwhaleSetupPopupView::default()
        }
    }

    /// Builds deterministic public pod rows including registration availability.
    fn picker() -> FunkwhalePodPickerView {
        FunkwhalePodPickerView {
            pods: (0..30)
                .map(|index| FunkwhalePodView {
                    label: format!("Pod {index}"),
                    url: format!("https://pod-{index}.example/"),
                    open_registrations: Some(index % 2 == 0),
                })
                .collect(),
            selected: 29,
            ..FunkwhalePodPickerView::default()
        }
    }

    /// Mouse events name the exact physical control cell without inherited modifiers.
    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Visible controls dispatch only setup actions, with saving distinct from discovery.
    #[test]
    fn manual_popup_displays_url_path_error_and_independent_buttons() {
        let popup = FunkwhaleSetupPopupView {
            error: Some("Invalid pod URL".to_owned()),
            ..setup()
        };
        let (buffer, hits) = render(&popup, 110, 30);
        let text: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !text.contains('▏'),
            "the pod editor cursor must use basic TTY glyphs"
        );
        for expected in [
            "Funkwhale pod setup",
            "https://music.example/",
            "/tmp/youta-test/config.toml",
            "Invalid pod URL",
        ] {
            assert!(text.contains(expected), "missing {expected:?}");
        }
        for expected in [
            UiAction::OpenFunkwhalePodPicker,
            UiAction::SaveFunkwhaleSetup,
            UiAction::CloseFunkwhaleSetup,
        ] {
            let (_, area) = hits
                .funkwhale_setup_buttons
                .iter()
                .find(|(action, _)| *action == expected)
                .unwrap();
            assert_eq!(
                mouse_action(mouse(MouseEventKind::Down(MouseButton::Left), *area), &hits),
                Some(expected)
            );
            assert_ne!(
                buffer[(area.x, area.y)].symbol(),
                " ",
                "a control must cover a displayed label"
            );
        }
        assert!(hits.funkwhale_pod_rows.is_empty());
    }

    /// Clearing the optional address remains an explicit, clickable save operation.
    #[test]
    fn empty_pod_draft_can_clear_the_saved_pod_through_the_save_button() {
        let popup = FunkwhaleSetupPopupView {
            url: String::new(),
            ..setup()
        };
        let (_, hits) = render(&popup, 110, 30);
        let (_, area) = hits
            .funkwhale_setup_buttons
            .iter()
            .find(|(action, _)| *action == UiAction::SaveFunkwhaleSetup)
            .expect("an empty draft clears the optional pod when explicitly saved");
        assert_eq!(
            mouse_action(mouse(MouseEventKind::Down(MouseButton::Left), *area), &hits),
            Some(UiAction::SaveFunkwhaleSetup)
        );
    }

    /// The selected public pod stays visible; the chooser cannot save through stale setup hits.
    #[test]
    fn picker_scrolls_selected_pod_and_registers_only_picker_controls() {
        let popup = FunkwhaleSetupPopupView {
            picker: Some(picker()),
            ..setup()
        };
        let (buffer, hits) = render(&popup, 110, 30);
        let text: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("Pod 29"));
        assert!(text.contains("registration closed"));
        assert!(
            hits.funkwhale_setup_buttons
                .iter()
                .all(|(action, _)| matches!(
                    action,
                    UiAction::OpenFunkwhalePodPicker | UiAction::CloseFunkwhalePodPicker
                ))
        );
        let (_, selected) = hits
            .funkwhale_pod_rows
            .iter()
            .find(|(index, _)| *index == 29)
            .unwrap();
        assert_eq!(
            mouse_action(
                mouse(MouseEventKind::Down(MouseButton::Left), *selected),
                &hits
            ),
            Some(UiAction::SelectFunkwhalePod(29))
        );
        assert_eq!(
            mouse_action(mouse(MouseEventKind::ScrollDown, *selected), &hits),
            Some(UiAction::MoveFunkwhalePod(1))
        );
        assert_eq!(
            mouse_action(mouse(MouseEventKind::ScrollUp, *selected), &hits),
            Some(UiAction::MoveFunkwhalePod(-1))
        );
        assert_eq!(
            mouse_action(mouse(MouseEventKind::Moved, *selected), &hits),
            None
        );
        assert_eq!(
            mouse_action(
                mouse(MouseEventKind::Down(MouseButton::Right), *selected),
                &hits
            ),
            None
        );
        assert_eq!(
            mouse_action(
                mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    Rect::new(0, 0, 1, 1)
                ),
                &hits
            ),
            None
        );
    }

    /// Loading, failed and empty directory states expose retry/close but no stale pod rows.
    #[test]
    fn unavailable_picker_states_keep_only_visible_retry_and_close_actions() {
        for (picker, expected) in [
            (
                FunkwhalePodPickerView {
                    loading: true,
                    ..picker()
                },
                "Loading public pods",
            ),
            (
                FunkwhalePodPickerView {
                    error: Some("Directory unavailable".to_owned()),
                    ..picker()
                },
                "Directory unavailable",
            ),
            (FunkwhalePodPickerView::default(), "No public pods"),
        ] {
            let popup = FunkwhaleSetupPopupView {
                picker: Some(picker),
                ..setup()
            };
            let (buffer, hits) = render(&popup, 110, 30);
            let text: String = buffer
                .content
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            assert!(text.contains(expected));
            assert!(hits.funkwhale_pod_rows.is_empty());
            assert_eq!(hits.funkwhale_setup_buttons.len(), 2);
        }
    }

    /// Layout clipping must never register invisible or out-of-frame mouse targets.
    #[test]
    fn tiny_terminals_keep_all_hits_inside_the_visible_frame() {
        for (width, height) in [(1, 1), (4, 3), (10, 5), (20, 8), (40, 12), (80, 24)] {
            for picker in [None, Some(picker())] {
                let popup = FunkwhaleSetupPopupView { picker, ..setup() };
                let (buffer, hits) = render(&popup, width, height);
                for area in hits
                    .funkwhale_setup_buttons
                    .iter()
                    .map(|(_, area)| area)
                    .chain(hits.funkwhale_pod_rows.iter().map(|(_, area)| area))
                {
                    assert!(!area.is_empty());
                    assert_eq!(
                        area.intersection(buffer.area),
                        *area,
                        "{width}x{height}: {area:?}"
                    );
                }
            }
        }
    }
}
