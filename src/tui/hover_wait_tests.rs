//! Notification-loop regressions for passive pointer motion and fixed tick deadlines.

use super::*;
use std::collections::VecDeque;

/// Supplies a passive pointer event, not a click or an application action.
fn motion(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// Constructs a small unselected tab without depending on feature-gated source catalogues.
fn tab_fixture() -> (ViewModel, HitMap, hover::HoverState, VirtualCursor) {
    let view = ViewModel {
        screen: Screen::Search,
        ..ViewModel::default()
    };
    let hit_map = HitMap {
        tabs: vec![(Screen::Statistics, Rect::new(10, 0, 5, 1))],
        ..HitMap::default()
    };
    (
        view,
        hit_map,
        hover::HoverState::default(),
        VirtualCursor::default(),
    )
}

/// The actual tab renderer and overlay agree on hitboxes without changing labels or selection.
#[test]
fn real_frame_hover_changes_only_the_inactive_tab_colors() {
    let view = ViewModel::default();
    let settings = UiSettings::default();
    let mut hit_map = HitMap::default();
    let mut hover = hover::HoverState::default();
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(200, 24)).unwrap();
    terminal
        .draw(|frame| render_frame(frame, &view, &settings, &mut hit_map, None))
        .unwrap();
    let before = terminal.backend().buffer().clone();
    let area = hit_map
        .tabs
        .iter()
        .find(|(screen, _)| *screen != view.displayed_screen())
        .unwrap()
        .1;
    assert!(hover.update_mouse(motion(area.x, area.y), &hit_map, &view));
    terminal
        .draw(|frame| {
            render_frame(frame, &view, &settings, &mut hit_map, None);
            hover.render(frame, &hit_map, &view);
        })
        .unwrap();
    let after = terminal.backend().buffer();
    for y in 0..24 {
        for x in 0..200 {
            let before_cell = &before[(x, y)];
            let after_cell = &after[(x, y)];
            assert_eq!(before_cell.symbol(), after_cell.symbol());
            assert_eq!(before_cell.modifier, after_cell.modifier);
            if contains(area, x, y) {
                assert_eq!(after_cell.bg, Color::Gray);
                assert_eq!(after_cell.fg, Color::Black);
            } else {
                assert_eq!(before_cell.bg, after_cell.bg);
                assert_eq!(before_cell.fg, after_cell.fg);
            }
        }
    }
    assert_eq!(view.screen, Screen::Search);
}

/// Repeated movement within a control is consumed until a meaningful event wakes the frame.
#[test]
fn unchanged_motion_waits_for_keys_without_resetting_the_tick_budget() {
    let (view, hit_map, mut hover, mut cursor) = tab_fixture();
    assert!(hover.update_mouse(motion(10, 0), &hit_map, &view));
    let key = Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let mut events = VecDeque::from([
        Some(Event::Mouse(motion(11, 0))),
        Some(Event::Mouse(motion(12, 0))),
        Some(key.clone()),
    ]);
    let mut previous_wait = Duration::from_secs(1);
    let mut calls = 0;
    let result = wait_for_visible_terminal_event(
        previous_wait,
        &mut hover,
        &mut cursor,
        &hit_map,
        &view,
        |remaining| {
            assert!(
                remaining <= previous_wait,
                "motion must not restart the timer"
            );
            previous_wait = remaining;
            calls += 1;
            Ok(events.pop_front().expect("queued fixture event"))
        },
    )
    .unwrap();
    assert_eq!(result, Some(key));
    assert_eq!(calls, 3);
}

/// Entering and leaving one control must each wake the frame immediately.
#[test]
fn hover_boundary_changes_wake_the_frame_without_waiting_for_a_tick() {
    let (view, hit_map, mut hover, mut cursor) = tab_fixture();
    for mouse in [motion(10, 0), motion(0, 0)] {
        let mut calls = 0;
        let result = wait_for_visible_terminal_event(
            Duration::from_secs(1),
            &mut hover,
            &mut cursor,
            &hit_map,
            &view,
            |_| {
                calls += 1;
                assert_eq!(calls, 1, "hover changes must not read another event");
                Ok(Some(Event::Mouse(mouse)))
            },
        )
        .unwrap();
        assert_eq!(result, Some(Event::Mouse(mouse)));
    }
}

/// A steady motion stream cannot starve playback ticks once their deadline is due.
#[test]
fn expired_tick_budget_stops_unchanged_motion_after_one_read() {
    let (view, hit_map, mut hover, mut cursor) = tab_fixture();
    let mut calls = 0;
    let result = wait_for_visible_terminal_event(
        Duration::ZERO,
        &mut hover,
        &mut cursor,
        &hit_map,
        &view,
        |remaining| {
            assert_eq!(remaining, Duration::ZERO);
            calls += 1;
            assert_eq!(calls, 1);
            Ok(Some(Event::Mouse(motion(0, 1))))
        },
    )
    .unwrap();
    assert_eq!(result, None);
}

/// Worker replies and artwork completion are not delayed by visually unchanged motion.
#[test]
fn worker_ready_after_unchanged_motion_still_finishes_the_wait() {
    let (view, hit_map, mut hover, mut cursor) = tab_fixture();
    let mut events = VecDeque::from([Some(Event::Mouse(motion(0, 1))), None]);
    let result = wait_for_visible_terminal_event(
        Duration::from_secs(1),
        &mut hover,
        &mut cursor,
        &hit_map,
        &view,
        |_| Ok(events.pop_front().expect("queued fixture completion")),
    )
    .unwrap();
    assert_eq!(result, None);
    assert!(events.is_empty());
}

/// The visible F8 pointer needs a new frame even within a single button's hitbox.
#[test]
fn active_keyboard_pointer_motion_is_not_coalesced_within_one_control() {
    let (view, hit_map, mut hover, mut cursor) = tab_fixture();
    assert!(hover.update_mouse(motion(10, 0), &hit_map, &view));
    cursor.active = true;
    cursor.bounds = Rect::new(0, 0, 80, 24);
    cursor.column = 10;
    let mouse = motion(11, 0);
    let result = wait_for_visible_terminal_event(
        Duration::from_secs(1),
        &mut hover,
        &mut cursor,
        &hit_map,
        &view,
        |_| Ok(Some(Event::Mouse(mouse))),
    )
    .unwrap();
    assert_eq!(result, Some(Event::Mouse(mouse)));
    assert_eq!(cursor.column, 11);
}

/// Focus tracking is enabled per session and removed before returning terminal ownership.
#[test]
fn terminal_hover_focus_tracking_is_balanced_across_start_exit_and_resume() {
    let mut output = Vec::new();
    write_terminal_startup(&mut output).unwrap();
    assert!(output.windows(8).any(|bytes| bytes == b"\x1b[?1004h"));
    output.clear();
    write_terminal_exit(&mut output).unwrap();
    assert!(output.windows(8).any(|bytes| bytes == b"\x1b[?1004l"));
    output.clear();
    write_terminal_startup(&mut output).unwrap();
    assert_eq!(
        output
            .windows(8)
            .filter(|bytes| *bytes == b"\x1b[?1004h")
            .count(),
        1
    );
}
