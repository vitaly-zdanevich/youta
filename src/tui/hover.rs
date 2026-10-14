//! Terminal-local hover feedback for enabled tabs and explicit controls.

use super::{
    Color, Frame, HitMap, Modifier, MouseButton, MouseEvent, MouseEventKind, Rect, UiAction,
    ViewModel, contains, mouse_action,
};

/// Keeps pointer feedback outside the shared application model.
#[derive(Default)]
pub(super) struct HoverState {
    /// Latest physical pointer report, including native-selection modifiers.
    pointer: Option<MouseEvent>,
    /// Last resolved control, used to avoid redraws within one hit rectangle.
    target: Option<Rect>,
}

impl HoverState {
    /// Remembers pointer movement and reports whether its control changed.
    pub(super) fn update_mouse(
        &mut self,
        mouse: MouseEvent,
        hit_map: &HitMap,
        view: &ViewModel,
    ) -> bool {
        self.pointer = Some(mouse);
        let target = control_at_pointer(mouse, hit_map, view);
        let changed = target != self.target;
        self.target = target;
        changed
    }

    /// Clears feedback when the terminal loses ownership of the pointer.
    pub(super) fn clear(&mut self) -> bool {
        self.pointer = None;
        self.target.take().is_some()
    }

    /// Applies feedback using the fresh frame's geometry and existing styles.
    pub(super) fn render(&mut self, frame: &mut Frame<'_>, hit_map: &HitMap, view: &ViewModel) {
        self.target = self
            .pointer
            .and_then(|mouse| control_at_pointer(mouse, hit_map, view));
        let Some(area) = self.target.map(|target| target.intersection(frame.area())) else {
            return;
        };
        if area.is_empty() {
            return;
        }
        // Selected controls and custom styles own their complete background.
        // Underlined text targets in legacy button vectors remain content links.
        let buffer = frame.buffer_mut();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let cell = &buffer[(x, y)];
                if cell.bg != Color::Reset
                    || cell.fg == Color::DarkGray
                    || cell
                        .modifier
                        .intersects(Modifier::REVERSED | Modifier::UNDERLINED)
                {
                    return;
                }
            }
        }
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].set_bg(Color::DarkGray);
            }
        }
    }
}

/// Resolves only explicit controls using the existing modal and capability policy.
///
/// A simulated click is inspected without dispatching it. Matching its routed
/// action to a control's recorded rectangle prevents obscured controls, fields,
/// artwork, seek tracks, and selectable content from acquiring hover feedback.
fn control_at_pointer(mouse: MouseEvent, hit_map: &HitMap, view: &ViewModel) -> Option<Rect> {
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        ..mouse
    };
    let action = mouse_action(click, hit_map, view)?;
    if !is_button_action(&action) {
        return None;
    }
    let matches_control = |(candidate, area): &(UiAction, Rect)| {
        (candidate == &action && contains(*area, mouse.column, mouse.row)).then_some(*area)
    };
    // Modal controls are considered before body controls when rectangles overlap.
    let popup_buttons = [
        &hit_map.bug_report_buttons,
        &hit_map.error_buttons,
        &hit_map.unsubscribe_buttons,
        &hit_map.site_file_buttons,
        #[cfg(feature = "s3-upload")]
        &hit_map.s3_credentials_buttons,
        #[cfg(feature = "s3-upload")]
        &hit_map.s3_upload_buttons,
        #[cfg(feature = "archive-upload")]
        &hit_map.archive_credentials_buttons,
        #[cfg(feature = "archive-upload")]
        &hit_map.archive_upload_buttons,
        #[cfg(feature = "commons-upload")]
        &hit_map.commons_credentials_buttons,
        #[cfg(feature = "commons-upload")]
        &hit_map.commons_upload_buttons,
        #[cfg(feature = "evernote")]
        &hit_map.evernote_credentials_buttons,
        #[cfg(feature = "evernote")]
        &hit_map.evernote_buttons,
        &hit_map.audio_quality_buttons,
        &hit_map.video_summary_buttons,
        &hit_map.project_history_buttons,
        #[cfg(feature = "lan-sharing")]
        &hit_map.podcast_feed_options_buttons,
        #[cfg(feature = "lan-sharing")]
        &hit_map.lan_share_buttons,
        #[cfg(feature = "qr")]
        &hit_map.video_qr_buttons,
        &hit_map.video_comments_buttons,
        &hit_map.private_note_buttons,
        &hit_map.queue_popup_buttons,
        &hit_map.playlist_popup_buttons,
        &hit_map.archive_playback_choice_buttons,
        &hit_map.download_choice_buttons,
        &hit_map.download_queue_buttons,
        #[cfg(feature = "yt-dlp")]
        &hit_map.channel_download_buttons,
        &hit_map.local_file_buttons,
        &hit_map.rss_subscription_buttons,
        &hit_map.yandex_music_setup_buttons,
        #[cfg(feature = "funkwhale")]
        &hit_map.funkwhale_setup_buttons,
        &hit_map.youtube_setup_buttons,
        &hit_map.preferences_buttons,
    ];
    if let Some(area) = popup_buttons
        .into_iter()
        .flatten()
        .find_map(matches_control)
    {
        return Some(area);
    }
    // Help has no pointer buttons and overlays the normal tab/body controls.
    if view.help_open {
        return None;
    }
    if let UiAction::ShowScreen(screen) = action {
        return hit_map.tabs.iter().find_map(|(candidate, area)| {
            (*candidate == screen
                && screen != view.displayed_screen()
                && contains(*area, mouse.column, mouse.row))
            .then_some(*area)
        });
    }
    [
        &hit_map.buttons,
        &hit_map.detail_buttons,
        &hit_map.subscription_source_buttons,
    ]
    .into_iter()
    .flatten()
    .find_map(matches_control)
}

/// Excludes content/list entries stored alongside controls in legacy hit vectors.
fn is_button_action(action: &UiAction) -> bool {
    !matches!(
        action,
        UiAction::SelectInvidiousInstance(_)
            | UiAction::SelectFunkwhalePod(_)
            | UiAction::SelectDownloadChoice { .. }
            | UiAction::SelectArchivePlaybackChoice { .. }
            | UiAction::SelectDownloadQueueEntry(_)
            | UiAction::OpenWikidataValue(_)
            | UiAction::OpenUrlInfo(_)
            | UiAction::ActivateTimecode { .. }
            | UiAction::OpenVideoCommentAuthor(_)
            | UiAction::ActivateCommentEmail { .. }
    )
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::tui::{Color, KeyModifiers, Modifier, MouseEventKind, Screen, Style, UiAction};
    use crate::view::{ErrorPopupView, LocalFileProgressView, MediaChoicePopupView};

    /// Supplies real terminal mouse events without requiring an attached terminal.
    fn moved(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Paints a styled mock control, then runs the production hover overlay.
    fn paint(
        hover: &mut HoverState,
        hit_map: &HitMap,
        view: &ViewModel,
        area: Rect,
        style: Style,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).expect("terminal");
        terminal
            .draw(|frame| {
                for y in area.y..area.bottom().min(frame.area().bottom()) {
                    for x in area.x..area.right().min(frame.area().right()) {
                        frame.buffer_mut()[(x, y)].set_symbol("x").set_style(style);
                    }
                }
                hover.render(frame, hit_map, view);
            })
            .expect("draw hover fixture");
        terminal
    }

    #[test]
    fn inactive_tab_receives_background_on_exact_label_cells() {
        let view = ViewModel::default();
        let area = Rect::new(2, 0, 5, 1);
        let hit_map = HitMap {
            tabs: vec![(Screen::History, area)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(3, 0), &hit_map, &view));
        let terminal = paint(&mut hover, &hit_map, &view, area, Style::default());
        for x in area.x..area.right() {
            assert_eq!(terminal.backend().buffer()[(x, 0)].bg, Color::DarkGray);
        }
        assert_eq!(terminal.backend().buffer()[(1, 0)].bg, Color::Reset);
        assert_eq!(terminal.backend().buffer()[(7, 0)].bg, Color::Reset);
        assert_eq!(terminal.backend().buffer()[(3, 1)].bg, Color::Reset);
    }

    #[test]
    fn button_feedback_preserves_foreground_and_modifiers() {
        let view = ViewModel::default();
        let area = Rect::new(2, 2, 5, 1);
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, area)],
            ..HitMap::default()
        };
        let style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC);
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(3, 2), &hit_map, &view));
        let terminal = paint(&mut hover, &hit_map, &view, area, style);
        let cell = &terminal.backend().buffer()[(3, 2)];
        assert_eq!(cell.bg, Color::DarkGray);
        assert_eq!(cell.fg, Color::Cyan);
        assert_eq!(cell.modifier, Modifier::BOLD | Modifier::ITALIC);
    }

    #[test]
    fn motion_within_one_control_does_not_request_another_redraw() {
        let view = ViewModel::default();
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, Rect::new(2, 2, 5, 1))],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(2, 2), &hit_map, &view));
        assert!(!hover.update_mouse(moved(5, 2), &hit_map, &view));
        assert!(hover.update_mouse(moved(7, 2), &hit_map, &view));
        assert!(!hover.update_mouse(moved(8, 2), &hit_map, &view));
        assert!(!hover.clear());
        assert!(hover.update_mouse(moved(4, 2), &hit_map, &view));
        assert!(hover.clear());
        assert!(!hover.clear());
    }

    #[test]
    fn selected_tabs_and_button_backgrounds_keep_their_styles() {
        let view = ViewModel::default();
        let area = Rect::new(2, 2, 5, 1);
        let active = HitMap {
            tabs: vec![(view.displayed_screen(), area)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(!hover.update_mouse(moved(3, 2), &active, &view));
        let buttons = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, area)],
            ..HitMap::default()
        };
        for style in [
            Style::default().fg(Color::Black).bg(Color::Cyan),
            Style::default().fg(Color::Green).bg(Color::Blue),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::REVERSED),
            Style::default().fg(Color::DarkGray),
        ] {
            hover.update_mouse(moved(3, 2), &buttons, &view);
            let terminal = paint(&mut hover, &buttons, &view, area, style);
            let mut expected = ratatui::buffer::Cell::default();
            expected.set_symbol("x").set_style(style);
            assert_eq!(&terminal.backend().buffer()[(3, 2)], &expected);
        }
    }

    #[test]
    fn rows_artwork_links_seek_track_and_content_do_not_hover() {
        let view = ViewModel::default();
        let area = Rect::new(2, 2, 5, 1);
        let mut hover = HoverState::default();
        for hit_map in [
            HitMap {
                rows: area,
                ..HitMap::default()
            },
            HitMap {
                thumbnail_area: Some(area),
                ..HitMap::default()
            },
            HitMap {
                detail_links: vec![(0, area)],
                ..HitMap::default()
            },
            HitMap {
                details_panel: area,
                ..HitMap::default()
            },
            HitMap {
                seek_bar: area,
                ..HitMap::default()
            },
            HitMap {
                now_playing: Some(area),
                ..HitMap::default()
            },
        ] {
            assert!(!hover.update_mouse(moved(3, 2), &hit_map, &view));
            let terminal = paint(&mut hover, &hit_map, &view, area, Style::default());
            assert_eq!(terminal.backend().buffer()[(3, 2)].bg, Color::Reset);
        }
    }

    #[test]
    fn mixed_button_vectors_exclude_list_rows_and_content_links() {
        let view = ViewModel::default();
        let area = Rect::new(2, 2, 5, 1);
        let mut hover = HoverState::default();
        for action in [
            UiAction::SelectInvidiousInstance(0),
            UiAction::SelectDownloadChoice {
                generation: 1,
                index: 0,
            },
            UiAction::SelectArchivePlaybackChoice {
                generation: 1,
                index: 0,
            },
            UiAction::SelectDownloadQueueEntry(1),
            UiAction::OpenWikidataValue("https://example.com".to_owned()),
            UiAction::OpenUrlInfo(0),
            UiAction::ActivateTimecode {
                media_id: crate::domain::MediaId::new(crate::domain::SourceKind::YouTube, "test"),
                seconds: 1,
            },
            UiAction::OpenVideoCommentAuthor(0),
        ] {
            let hit_map = HitMap {
                detail_buttons: vec![(action, area)],
                ..HitMap::default()
            };
            assert!(!hover.update_mouse(moved(3, 2), &hit_map, &view));
        }
    }

    #[test]
    fn underlined_urls_in_button_vectors_keep_their_link_styling() {
        let view = ViewModel::default();
        let area = Rect::new(2, 2, 5, 1);
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::OpenChannelInBrowser, area)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        hover.update_mouse(moved(3, 2), &hit_map, &view);
        let style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::UNDERLINED);
        let terminal = paint(&mut hover, &hit_map, &view, area, style);
        let mut expected = ratatui::buffer::Cell::default();
        expected.set_symbol("x").set_style(style);
        assert_eq!(&terminal.backend().buffer()[(3, 2)], &expected);
    }

    #[test]
    fn original_page_button_keeps_hover_even_when_its_label_contains_a_url() {
        let view = ViewModel::default();
        let area = Rect::new(2, 2, 25, 1);
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::OpenInBrowser, area)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(3, 2), &hit_map, &view));
        let terminal = paint(
            &mut hover,
            &hit_map,
            &view,
            area,
            Style::default().fg(Color::Cyan),
        );
        assert_eq!(terminal.backend().buffer()[(3, 2)].bg, Color::DarkGray);
    }

    #[test]
    fn shift_motion_clears_feedback_without_overriding_native_text_selection() {
        let view = ViewModel::default();
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, Rect::new(2, 2, 5, 1))],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(3, 2), &hit_map, &view));
        assert!(hover.update_mouse(
            MouseEvent {
                modifiers: KeyModifiers::SHIFT,
                ..moved(3, 2)
            },
            &hit_map,
            &view
        ));
        assert!(!hover.clear());
    }

    #[test]
    fn topmost_popup_hovers_its_button_and_blocks_underlying_tabs() {
        let view = ViewModel {
            error_popup: Some(ErrorPopupView::default()),
            ..ViewModel::default()
        };
        let tab = Rect::new(2, 0, 5, 1);
        let modal = Rect::new(2, 2, 5, 1);
        let hit_map = HitMap {
            tabs: vec![(Screen::History, tab)],
            error_buttons: vec![(UiAction::CopyErrorReport, modal)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(!hover.update_mouse(moved(3, 0), &hit_map, &view));
        assert!(hover.update_mouse(moved(3, 2), &hit_map, &view));
        let terminal = paint(
            &mut hover,
            &hit_map,
            &view,
            modal,
            Style::default().fg(Color::Cyan),
        );
        assert_eq!(terminal.backend().buffer()[(3, 2)].bg, Color::DarkGray);
    }

    #[test]
    fn a_modal_button_wins_over_an_overlapping_underlying_tab() {
        let view = ViewModel {
            error_popup: Some(ErrorPopupView::default()),
            ..ViewModel::default()
        };
        let modal = Rect::new(4, 0, 3, 1);
        let hit_map = HitMap {
            tabs: vec![(Screen::History, Rect::new(2, 0, 9, 1))],
            error_buttons: vec![(UiAction::CopyErrorReport, modal)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(5, 0), &hit_map, &view));
        let terminal = paint(
            &mut hover,
            &hit_map,
            &view,
            Rect::new(2, 0, 9, 1),
            Style::default(),
        );
        assert_eq!(terminal.backend().buffer()[(5, 0)].bg, Color::DarkGray);
        assert_eq!(terminal.backend().buffer()[(3, 0)].bg, Color::Reset);
        assert_eq!(terminal.backend().buffer()[(8, 0)].bg, Color::Reset);
    }

    #[test]
    fn disabled_external_actions_and_help_leave_underlying_controls_untouched() {
        let area = Rect::new(2, 2, 5, 1);
        let mut hover = HoverState::default();
        let unavailable = ViewModel {
            external_opener_available: false,
            ..ViewModel::default()
        };
        let external = HitMap {
            detail_buttons: vec![(UiAction::OpenInBrowser, area)],
            ..HitMap::default()
        };
        assert!(!hover.update_mouse(moved(3, 2), &external, &unavailable));
        let help = ViewModel {
            help_open: true,
            ..ViewModel::default()
        };
        let buttons = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, area)],
            ..HitMap::default()
        };
        assert!(!hover.update_mouse(moved(3, 2), &buttons, &help));
        let transferring = ViewModel {
            local_file_progress: Some(LocalFileProgressView::default()),
            ..ViewModel::default()
        };
        assert!(!hover.update_mouse(moved(3, 2), &buttons, &transferring));
    }

    #[test]
    fn modal_choices_hover_confirm_buttons_without_hovering_option_rows() {
        let popup = MediaChoicePopupView {
            generation: 1,
            title: "Mock download".to_owned(),
            explanation: "Choose one format".to_owned(),
            options: vec!["Audio".to_owned(), "Video".to_owned()],
            selected: 0,
        };
        let view = ViewModel {
            download_choice_popup: Some(popup),
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            download_choice_buttons: vec![
                (
                    UiAction::SelectDownloadChoice {
                        generation: 1,
                        index: 1,
                    },
                    Rect::new(2, 2, 5, 1),
                ),
                (UiAction::ConfirmDownloadChoice(1), Rect::new(2, 4, 5, 1)),
            ],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(!hover.update_mouse(moved(3, 2), &hit_map, &view));
        assert!(hover.update_mouse(moved(3, 4), &hit_map, &view));
    }

    #[test]
    fn fresh_frame_geometry_prevents_hover_from_repainting_a_stale_target() {
        let view = ViewModel::default();
        let old = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, Rect::new(2, 2, 5, 1))],
            ..HitMap::default()
        };
        let new = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, Rect::new(2, 4, 5, 1))],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(3, 2), &old, &view));
        let terminal = paint(
            &mut hover,
            &new,
            &view,
            Rect::new(2, 2, 5, 1),
            Style::default(),
        );
        assert_eq!(terminal.backend().buffer()[(3, 2)].bg, Color::Reset);
        assert!(!hover.clear());
    }

    #[test]
    fn a_new_modal_revalidates_the_existing_pointer_without_mouse_motion() {
        let mut view = ViewModel::default();
        let area = Rect::new(2, 2, 5, 1);
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, area)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(3, 2), &hit_map, &view));
        view.error_popup = Some(ErrorPopupView::default());
        let terminal = paint(&mut hover, &hit_map, &view, area, Style::default());
        assert_eq!(terminal.backend().buffer()[(3, 2)].bg, Color::Reset);
        assert!(!hover.clear());
    }

    #[test]
    fn offscreen_targets_are_clipped_before_accessing_the_terminal_buffer() {
        let view = ViewModel::default();
        let area = Rect::new(28, 4, 7, 4);
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, area)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(29, 4), &hit_map, &view));
        let terminal = paint(&mut hover, &hit_map, &view, area, Style::default());
        assert_eq!(terminal.backend().buffer()[(29, 4)].bg, Color::DarkGray);
        assert_eq!(terminal.backend().buffer()[(29, 5)].bg, Color::DarkGray);
    }

    #[test]
    fn clear_removes_the_retained_pointer_from_future_frames() {
        let view = ViewModel::default();
        let area = Rect::new(2, 2, 5, 1);
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, area)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(3, 2), &hit_map, &view));
        assert!(hover.clear());
        let terminal = paint(&mut hover, &hit_map, &view, area, Style::default());
        assert_eq!(terminal.backend().buffer()[(3, 2)].bg, Color::Reset);
    }

    #[test]
    fn one_custom_cell_preserves_the_complete_control_background() {
        let view = ViewModel::default();
        let area = Rect::new(2, 2, 5, 1);
        let hit_map = HitMap {
            detail_buttons: vec![(UiAction::CopyLink, area)],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(4, 2), &hit_map, &view));
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).expect("terminal");
        terminal
            .draw(|frame| {
                frame.buffer_mut()[(3, 2)].set_bg(Color::Blue);
                hover.render(frame, &hit_map, &view);
            })
            .expect("draw partially styled control");
        assert_eq!(terminal.backend().buffer()[(3, 2)].bg, Color::Blue);
        assert_eq!(terminal.backend().buffer()[(4, 2)].bg, Color::Reset);
    }

    #[test]
    fn popup_buttons_remain_available_above_the_help_overlay() {
        let view = ViewModel {
            help_open: true,
            error_popup: Some(ErrorPopupView::default()),
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            error_buttons: vec![(UiAction::CopyErrorReport, Rect::new(2, 2, 5, 1))],
            ..HitMap::default()
        };
        let mut hover = HoverState::default();
        assert!(hover.update_mouse(moved(3, 2), &hit_map, &view));
    }

    #[test]
    fn production_details_button_hovers_with_standard_and_funny_terminal_themes() {
        let view = ViewModel {
            details: Some(crate::view::DetailView {
                title: "Offline hover fixture".to_owned(),
                source: "YouTube".to_owned(),
                webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/watch?v=fixture").unwrap(),
                ),
                ..crate::view::DetailView::default()
            }),
            ..ViewModel::default()
        };
        for funny_mode in [false, true] {
            let settings = crate::tui::UiSettings {
                funny_mode,
                ..crate::tui::UiSettings::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
            let mut hit_map = HitMap::default();
            terminal
                .draw(|frame| crate::tui::render(frame, &view, &settings, &mut hit_map))
                .expect("draw real controls");
            let area = hit_map
                .detail_buttons
                .iter()
                .find_map(|(action, area)| (*action == UiAction::OpenInBrowser).then_some(*area))
                .expect("original page button");
            let mut expected = terminal.backend().buffer()[(area.x, area.y)].clone();
            expected.set_bg(Color::DarkGray);
            let mut hover = HoverState::default();
            assert!(hover.update_mouse(moved(area.x, area.y), &hit_map, &view));
            terminal
                .draw(|frame| {
                    crate::tui::render(frame, &view, &settings, &mut hit_map);
                    hover.render(frame, &hit_map, &view);
                })
                .expect("draw real button hover");
            let actual = &terminal.backend().buffer()[(area.x, area.y)];
            assert_eq!(actual.style(), expected.style());
            assert_eq!(actual.symbol(), expected.symbol());
        }
    }
}
