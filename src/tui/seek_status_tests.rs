//! Regression coverage for opposite-edge playback status and title alignment.

use super::*;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

/// Supplies the reported footer without network, credentials, or a player subprocess.
fn playing_view() -> ViewModel {
    ViewModel {
        playback: PlaybackStatus {
            idle: false,
            paused: false,
            position: Duration::from_secs(143),
            duration: Some(Duration::from_secs(881)),
            speed: 1.0,
            volume: 30,
            title: Some("The Orphans Of Piety".to_owned()),
            ..PlaybackStatus::default()
        },
        ..ViewModel::default()
    }
}

/// Reads one ASCII fixture row, including the blank space between the two sections.
fn row(terminal: &Terminal<TestBackend>, area: Rect) -> String {
    (area.x..area.right())
        .map(|x| terminal.backend().buffer()[(x, area.y)].symbol())
        .collect()
}

/// All timeline variants use the same edges, including the reserved loading-spinner cells.
#[test]
fn metadata_and_title_use_opposite_edges_in_every_seek_layout() {
    for (layout, height) in [
        ("normal", 2),
        ("compact", 1),
        ("live", 2),
        ("waveform", WAVEFORM_PLAYER_ROWS),
        ("chapters", 3),
    ] {
        for starting in [false, true] {
            let mut view = playing_view();
            view.playback_starting = starting;
            let prefix = match layout {
                "live" => {
                    view.playback.live = true;
                    "LIVE 2:23  vol 30%"
                }
                "waveform" => {
                    view.waveform_visible = true;
                    "2:23 / 14:41  vol 30%"
                }
                "chapters" => {
                    view.playback_chapters = vec![Chapter {
                        title: "Chapter".to_owned(),
                        start_seconds: 0,
                        end_seconds: None,
                    }];
                    "2:23 / 14:41  vol 30%"
                }
                _ => "2:23 / 14:41  vol 30%",
            };
            let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
            let mut hits = HitMap::default();
            terminal
                .draw(|frame| {
                    let theme = Theme::new(false);
                    render_seek_bar(
                        frame,
                        frame.area(),
                        &view,
                        &UiSettings::default(),
                        &theme,
                        &mut hits,
                    );
                    render_playback_start_activity(frame, &view, &theme);
                })
                .unwrap();
            let x = if starting { 2 } else { 0 };
            let status = row(&terminal, Rect::new(x, height - 1, 100 - x, 1));
            assert!(
                status.starts_with(prefix),
                "{layout}, starting={starting}: {status:?}"
            );
            assert!(
                status.ends_with("The Orphans Of Piety"),
                "{layout}: {status:?}"
            );
            let title = hits.now_playing.expect("visible title target");
            assert_eq!(title.right(), 100, "{layout}");
            assert_eq!(row(&terminal, title), "The Orphans Of Piety");
            for (column, expected) in [(title.x, Some(UiAction::ShowNowPlaying)), (x, None)] {
                let action = mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column,
                        row: title.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hits,
                    &view,
                );
                if expected.is_some() {
                    assert_eq!(action, expected);
                } else {
                    assert_ne!(action, Some(UiAction::ShowNowPlaying));
                }
            }
        }
    }
}

/// Right alignment uses rendered cell widths even when Unicode truncation leaves spare cells.
#[test]
fn long_unicode_titles_fit_without_displacing_metadata() {
    const PREFIX: &str = "2:23 / 14:41  vol 30%";
    let mut view = playing_view();
    view.playback.title = Some("曲目🦀e\u{301}曲目🦀e\u{301}曲目🦀e\u{301}".to_owned());
    for width in [0, 1, 2, 10, 24, 25, 26, 27, 28, 29, 32, 40, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
        let mut hits = HitMap::default();
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hits,
                )
            })
            .unwrap();
        let available = usize::from(width).saturating_sub(PREFIX.len() + 2);
        let title = truncate_terminal_text(view.playback.title.as_deref().unwrap(), available);
        let mut expected = Buffer::empty(Rect::new(0, 0, width, 2));
        expected.set_string(
            0,
            1,
            truncate_terminal_text(PREFIX, usize::from(width)),
            Style::default(),
        );
        if title.is_empty() {
            assert!(hits.now_playing.is_none(), "width={width}");
        } else {
            let title_width = terminal_text_width(&title);
            let target = Rect::new(width - title_width, 1, title_width, 1);
            assert_eq!(hits.now_playing, Some(target), "width={width}");
            expected.set_string(target.x, target.y, title, Style::default());
        }
        for x in 0..width {
            assert_eq!(
                terminal.backend().buffer()[(x, 1)].symbol(),
                expected[(x, 1)].symbol(),
                "width={width}, x={x}"
            );
        }
    }
}

/// A short status stays at the area's left edge when no active title exists.
#[test]
fn missing_or_idle_titles_keep_metadata_left_aligned_in_offset_areas() {
    for title in [None, Some(String::new()), Some("Old title".to_owned())] {
        let mut view = playing_view();
        view.playback.idle = title.as_deref() == Some("Old title");
        view.playback.title = title;
        let mut terminal = Terminal::new(TestBackend::new(90, 6)).unwrap();
        let mut hits = HitMap::default();
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    Rect::new(4, 2, 80, 2),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hits,
                )
            })
            .unwrap();
        assert!(row(&terminal, Rect::new(4, 3, 80, 1)).starts_with("2:23 / 14:41  vol 30%"));
        assert!(hits.now_playing.is_none());
    }
}

/// Overlaying the two compact labels must preserve the cached gauge between them.
#[test]
fn compact_status_preserves_the_seekbar_between_its_labels() {
    let mut view = playing_view();
    view.playback.buffered_ranges = vec![crate::playback::BufferedRange {
        start: Duration::ZERO,
        end: Duration::from_secs(881),
    }];
    let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
    let mut hits = HitMap::default();
    terminal
        .draw(|frame| {
            render_seek_bar(
                frame,
                frame.area(),
                &view,
                &UiSettings::default(),
                &Theme::new(false),
                &mut hits,
            )
        })
        .unwrap();
    assert!(row(&terminal, Rect::new(0, 0, 100, 1)).starts_with("2:23 / 14:41  vol 30%"));
    let title = hits.now_playing.unwrap();
    assert_eq!(title.right(), 100);
    assert_eq!(
        terminal.backend().buffer()[(50, 0)].symbol(),
        ratatui::symbols::block::FULL
    );
    assert_ne!(
        mouse_action(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 50,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            &hits,
            &view
        ),
        Some(UiAction::ShowNowPlaying)
    );
}

/// Moved compact labels retain the gauge's contrasting text on played backgrounds.
#[test]
fn compact_labels_preserve_played_backgrounds_in_both_themes() {
    for dark in [false, true] {
        let mut view = playing_view();
        view.playback.position = Duration::from_secs(95);
        view.playback.duration = Some(Duration::from_secs(100));
        let theme = Theme::new(dark);
        let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
        let mut hits = HitMap::default();
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &theme,
                    &mut hits,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for x in [0, hits.now_playing.unwrap().x] {
            assert_eq!(buffer[(x, 0)].fg, theme.progress.bg.unwrap_or(Color::Reset));
            assert_eq!(buffer[(x, 0)].bg, theme.progress.fg.unwrap_or(Color::Reset));
        }
        assert_eq!(
            buffer[(99, 0)].bg,
            theme.progress.bg.unwrap_or(Color::Reset)
        );
        assert_eq!(buffer[(50, 0)].symbol(), ratatui::symbols::block::FULL);
        assert_eq!(
            buffer[(50, 0)].bg,
            theme.progress.bg.unwrap_or(Color::Reset)
        );
    }
}
