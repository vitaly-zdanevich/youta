//! Paused chapter rendering retains an explicit seek despite mpv's audio-buffer offset.

use super::*;
use crate::view::DetailTimecodeView;
use ratatui::backend::TestBackend;

/// Models the native Opus reproduction without a player process or network calls.
fn paused_chapter_view() -> ViewModel {
    let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
    let description = "0:00 Introduction\n1:35 Main\n2:30 Finish";
    ViewModel {
        playback: PlaybackStatus {
            idle: false,
            paused: true,
            position: Duration::from_micros(94_806_876),
            duration: Some(Duration::from_secs(180)),
            ..PlaybackStatus::default()
        },
        paused_chapter_seek: Some((media_id.clone(), Duration::from_secs(95))),
        playing_media_id: Some(media_id.clone()),
        playback_chapters: [
            ("Introduction", 0, 95),
            ("Main", 95, 150),
            ("Finish", 150, 180),
        ]
        .into_iter()
        .map(|(title, start_seconds, end)| Chapter {
            title: title.to_owned(),
            start_seconds,
            end_seconds: Some(end),
        })
        .collect(),
        show_chapter_timestamps: false,
        details: Some(DetailView {
            media_id: Some(media_id),
            description: description.to_owned(),
            timecodes: [("0:00", 0), ("1:35", 95), ("2:30", 150)]
                .into_iter()
                .map(|(timestamp, seconds)| {
                    let start_byte = description.find(timestamp).expect("fixture timestamp");
                    DetailTimecodeView {
                        start_byte,
                        end_byte: start_byte + timestamp.len(),
                        seconds,
                        is_chapter: true,
                    }
                })
                .collect(),
            ..DetailView::default()
        }),
        ..ViewModel::default()
    }
}

/// The description's active timestamp and title use the same logical chapter position.
#[test]
fn paused_seek_highlights_the_requested_description_chapter() {
    let view = paused_chapter_view();
    let details = view.details.as_ref().unwrap();
    let active = active_description_chapter_line(&view, details).expect("active chapter line");
    assert_eq!(&details.description[active], "1:35 Main");
    assert_eq!(view.playback.position, Duration::from_micros(94_806_876));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let mut hits = HitMap::default();
    terminal
        .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hits))
        .unwrap();
    for (seconds, active) in [(0, false), (95, true), (150, false)] {
        let expected = UiAction::ActivateTimecode {
            media_id: view.playing_media_id.clone().unwrap(),
            seconds,
        };
        let (_, area) = hits
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &expected)
            .expect("description timestamp target");
        for x in [area.x, area.right() + 1] {
            let cell = &terminal.backend().buffer()[(x, area.y)];
            assert_eq!(cell.modifier.contains(Modifier::BOLD), active);
            if active {
                assert_eq!(cell.fg, Color::Magenta);
            }
        }
    }
}

/// Current, previous and next labels retain exact identity-bound chapter targets.
#[test]
fn paused_seek_renders_the_requested_chapter_and_its_adjacent_controls() {
    let view = paused_chapter_view();
    let mut terminal = Terminal::new(TestBackend::new(120, 3)).unwrap();
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
            );
        })
        .unwrap();
    let layout = chapter_label_layout(&view, Duration::from_secs(180), 120, 1);
    for (index, text, seconds) in [
        (0, "◀ Introduction", 0),
        (1, "Main", 95),
        (2, "Finish ▶", 150),
    ] {
        let placement = layout
            .placements
            .iter()
            .find(|placement| placement.index == index)
            .expect("adjacent chapter placement");
        assert_eq!(placement.text, text);
        let expected = UiAction::ActivateTimecode {
            media_id: view.playing_media_id.clone().unwrap(),
            seconds,
        };
        let (_, area) = hits
            .seek_markers
            .iter()
            .find(|(action, area)| action == &expected && area.y < hits.seek_bar.y)
            .expect("chapter label target above the seek track");
        let rendered: String = (area.x..area.right())
            .map(|x| terminal.backend().buffer()[(x, area.y)].symbol())
            .collect();
        assert_eq!(rendered, text);
        for column in [area.x, area.right() - 1] {
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hits,
                    &view,
                ),
                Some(expected.clone()),
            );
        }
        let cell = &terminal.backend().buffer()[(area.x, area.y)];
        assert_eq!(cell.modifier.contains(Modifier::BOLD), seconds == 95);
    }
}

/// Ordinary playback and manual pauses must not highlight the following chapter early.
#[test]
fn an_unanchored_position_before_the_boundary_remains_in_the_previous_chapter() {
    for paused in [false, true] {
        let mut view = paused_chapter_view();
        view.playback.paused = paused;
        view.paused_chapter_seek = None;
        assert_eq!(view.chapter_position(), view.playback.position);
        let details = view.details.as_ref().unwrap();
        let active = active_description_chapter_line(&view, details).unwrap();
        assert_eq!(&details.description[active], "0:00 Introduction");
        let layout = chapter_label_layout(&view, Duration::from_secs(180), 120, 1);
        assert_eq!(layout.placements.len(), 2);
        assert!(
            layout
                .placements
                .iter()
                .any(|placement| placement.text == "Introduction")
        );
        assert!(
            layout
                .placements
                .iter()
                .any(|placement| placement.text == "Main ▶")
        );
    }
}

/// A retained presentation target cannot override another item, live media or resumed playback.
#[test]
fn paused_chapter_targets_are_limited_to_their_active_finite_item() {
    for invalidation in [
        "resumed",
        "idle",
        "live",
        "other item",
        "no item",
        "at end",
        "past end",
    ] {
        let mut view = paused_chapter_view();
        match invalidation {
            "resumed" => view.playback.paused = false,
            "idle" => view.playback.idle = true,
            "live" => view.playback.live = true,
            "other item" => {
                view.playing_media_id = Some(MediaId::new(SourceKind::YouTube, "lmnopqrstuv"))
            }
            "no item" => view.playing_media_id = None,
            "at end" => view.playback.duration = Some(Duration::from_secs(95)),
            "past end" => view.playback.duration = Some(Duration::from_secs(94)),
            _ => unreachable!("all invalidation cases are listed above"),
        }
        assert_eq!(
            view.chapter_position(),
            view.playback.position,
            "{invalidation}"
        );
    }
    let mut view = paused_chapter_view();
    view.playback.duration = None;
    assert_eq!(view.chapter_position(), Duration::from_secs(95));
    view.paused_chapter_seek.as_mut().unwrap().1 = Duration::ZERO;
    assert_eq!(view.chapter_position(), Duration::ZERO);
}

/// Reproduces an accepted 08:00 seek whose paused audio clock still reads 07:59.806.
fn paused_timestamp_view() -> ViewModel {
    let mut view = paused_chapter_view();
    view.playback.position = Duration::from_micros(479_806_876);
    view.playback.duration = Some(Duration::from_secs(640));
    view.paused_chapter_seek.as_mut().unwrap().1 = Duration::from_secs(480);
    view.playback_chapters.clear();
    view
}

/// All footer layouts show the accepted paused timestamp without rounding ordinary clocks.
#[test]
fn paused_timestamp_footer_shows_the_accepted_seek_in_every_layout() {
    for (height, waveform) in [(1, false), (2, false), (3, false), (5, true)] {
        for (paused, anchored, expected) in [
            (true, true, "8:00 / 10:40"),
            (true, false, "7:59 / 10:40"),
            (false, true, "7:59 / 10:40"),
        ] {
            let mut view = paused_timestamp_view();
            view.playback.paused = paused;
            if !anchored {
                view.paused_chapter_seek = None;
            }
            view.waveform_visible = waveform;
            if height == 3 {
                view.playback_chapters = vec![Chapter {
                    title: "Requested chapter".to_owned(),
                    start_seconds: 480,
                    end_seconds: Some(640),
                }];
            }
            let mut terminal = Terminal::new(TestBackend::new(102, height)).unwrap();
            terminal
                .draw(|frame| {
                    render_seek_bar(
                        frame,
                        frame.area(),
                        &view,
                        &UiSettings::default(),
                        &Theme::new(false),
                        &mut HitMap::default(),
                    );
                })
                .unwrap();
            let footer: String = (0..102)
                .map(|x| terminal.backend().buffer()[(x, height - 1)].symbol())
                .collect();
            assert!(
                footer.starts_with(expected),
                "height={height}, waveform={waveform}, paused={paused}, anchored={anchored}: {footer}"
            );
            assert_eq!(view.playback.position, Duration::from_micros(479_806_876));
        }
    }
}

/// Gauge fill and cached-range clipping agree at the accepted paused seek boundary.
#[test]
fn paused_timestamp_gauge_uses_the_same_position_as_its_footer() {
    for height in [1, 2] {
        for cached in [false, true] {
            let mut view = paused_timestamp_view();
            if cached {
                view.playback.buffered_ranges = vec![crate::playback::BufferedRange {
                    start: Duration::ZERO,
                    end: Duration::from_secs(640),
                }];
            }
            let theme = Theme::new(false);
            let mut terminal = Terminal::new(TestBackend::new(102, height)).unwrap();
            terminal
                .draw(|frame| {
                    render_seek_bar(
                        frame,
                        frame.area(),
                        &view,
                        &UiSettings::default(),
                        &theme,
                        &mut HitMap::default(),
                    );
                })
                .unwrap();
            // The exact target fills 77 cells; the native clock would fill only 76.
            let last_played = &terminal.backend().buffer()[(76, 0)];
            assert_eq!(last_played.symbol(), ratatui::symbols::block::FULL);
            assert_eq!(last_played.fg, theme.progress.fg.unwrap());
            if cached {
                assert_eq!(
                    terminal.backend().buffer()[(77, 0)].fg,
                    theme.cached.fg.unwrap()
                );
            }
        }
    }
}

/// Local waveform progress follows the same accepted paused seek as the footer.
#[test]
fn paused_timestamp_waveform_uses_the_accepted_seek_position() {
    let mut view = paused_chapter_view();
    let media_id = MediaId::new(SourceKind::Local, "/fixture/paused.opus");
    let duration = Duration::from_secs(190);
    view.playing_media_id = Some(media_id.clone());
    view.paused_chapter_seek = Some((media_id.clone(), Duration::from_secs(95)));
    view.playback.duration = Some(duration);
    view.waveform_playback_matches = true;
    view.waveform = WaveformView::Ready {
        media_id,
        generation: 1,
        duration,
        pyramid: std::sync::Arc::new(crate::waveform::PeakPyramid::from_peaks(
            vec![
                Peak {
                    minimum: 0,
                    maximum: i16::MAX
                };
                100
            ],
            1,
            100,
        )),
    };
    let theme = Theme::new(false);
    let mut terminal = Terminal::new(TestBackend::new(100, WAVEFORM_ROWS)).unwrap();
    terminal
        .draw(|frame| {
            render_waveform(frame, frame.area(), &view, &theme, &mut HitMap::default());
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(49, 0)].fg, theme.progress.fg.unwrap());
    assert_eq!(buffer[(50, 0)].fg, theme.muted.fg.unwrap());
    assert_eq!(view.playback.position, Duration::from_micros(94_806_876));
}

/// Models a timestamp queued after loading starts but before any audio is confirmed.
fn loading_chapter_view() -> ViewModel {
    let mut view = paused_chapter_view();
    view.loading_chapter_seek = view.paused_chapter_seek.take();
    view.playing_media_id = None;
    view.playback_starting = true;
    view.playback.idle = true;
    view.playback.paused = false;
    view.playback.position = Duration::ZERO;
    view
}

/// Loading feedback highlights the latest clicked chapter before audio starts.
#[test]
fn loading_timestamp_highlights_the_requested_description_before_audio_starts() {
    let mut view = loading_chapter_view();
    let media_id = view.details.as_ref().unwrap().media_id.clone().unwrap();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    for (seconds, expected_line) in [(95, "1:35 Main"), (150, "2:30 Finish")] {
        view.loading_chapter_seek = Some((media_id.clone(), Duration::from_secs(seconds)));
        let details = view.details.as_ref().unwrap();
        let active = active_description_chapter_line(&view, details)
            .expect("the queued timestamp owns the description highlight during loading");
        assert_eq!(&details.description[active], expected_line);
        let mut hits = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hits))
            .unwrap();
        for candidate in [0, 95, 150] {
            let expected = UiAction::ActivateTimecode {
                media_id: media_id.clone(),
                seconds: candidate,
            };
            let (_, area) = hits
                .detail_buttons
                .iter()
                .find(|(action, _)| action == &expected)
                .expect("description timestamp target remains clickable during loading");
            for x in [area.x, area.right() + 1] {
                let cell = &terminal.backend().buffer()[(x, area.y)];
                assert_eq!(cell.modifier.contains(Modifier::BOLD), candidate == seconds);
                if candidate == seconds {
                    assert_eq!(cell.fg, Color::Magenta);
                }
            }
        }
        assert_eq!(view.playing_media_id, None);
        assert!(view.playback.idle);
        assert_eq!(view.playback.position, Duration::ZERO);
    }
}

/// Loading chapter markers and labels retain the pending item's exact seek targets.
#[test]
fn loading_timestamp_chapter_controls_remain_clickable_before_audio_starts() {
    let view = loading_chapter_view();
    let media_id = view.details.as_ref().unwrap().media_id.clone().unwrap();
    let mut terminal = Terminal::new(TestBackend::new(120, 3)).unwrap();
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
            );
        })
        .unwrap();
    for seconds in [0, 95, 150] {
        let expected = UiAction::ActivateTimecode {
            media_id: media_id.clone(),
            seconds,
        };
        for marker_row in [false, true] {
            let (_, area) = hits
                .seek_markers
                .iter()
                .find(|(action, area)| {
                    action == &expected && (area.y == hits.seek_bar.y) == marker_row
                })
                .expect("both chapter labels and track markers use the loading item's identity");
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hits,
                    &view,
                ),
                Some(expected.clone()),
            );
            let cell = &terminal.backend().buffer()[(area.x, area.y)];
            assert_eq!(cell.modifier.contains(Modifier::BOLD), seconds == 95);
        }
    }
    let footer: String = (2..120)
        .map(|x| terminal.backend().buffer()[(x, 2)].symbol())
        .collect();
    assert!(footer.starts_with("1:35 / 3:00"), "{footer}");
    assert_eq!(view.playing_media_id, None);
    assert_eq!(view.playback.position, Duration::ZERO);
}

/// Stale or unrelated loading targets cannot mark a different description as active.
#[test]
fn loading_timestamp_highlights_require_a_current_matching_item() {
    for invalidation in [
        "cancelled",
        "cleared",
        "other details",
        "stale item",
        "active item",
        "live",
        "at end",
    ] {
        let mut view = loading_chapter_view();
        let other = MediaId::new(SourceKind::YouTube, "lmnopqrstuv");
        match invalidation {
            "cancelled" => view.playback_starting = false,
            "cleared" => view.loading_chapter_seek = None,
            "other details" => view.details.as_mut().unwrap().media_id = Some(other),
            "stale item" => {
                view.playback_starting = false;
                view.playing_media_id = Some(other);
            }
            "active item" => view.playing_media_id = Some(other),
            "live" => view.playback.live = true,
            "at end" => view.loading_chapter_seek.as_mut().unwrap().1 = Duration::from_secs(180),
            _ => unreachable!("all invalidation cases are listed above"),
        }
        assert!(
            active_description_chapter_line(&view, view.details.as_ref().unwrap()).is_none(),
            "{invalidation}"
        );
    }
}

/// Loading accepts identity-bound chapter clicks without enabling stale or generic seeks.
#[test]
fn loading_timestamp_clicks_reject_stale_or_generic_seek_targets() {
    for invalidation in [
        "other marker owner",
        "ready other marker owner",
        "generic marker",
        "cancelled",
        "cleared marker",
        "live",
        "at end",
        "active owner",
    ] {
        let mut view = loading_chapter_view();
        let media_id = view.details.as_ref().unwrap().media_id.clone().unwrap();
        let mut action = UiAction::ActivateTimecode {
            media_id: media_id.clone(),
            seconds: 95,
        };
        match invalidation {
            "other marker owner" | "ready other marker owner" => {
                view.playback.idle = invalidation != "ready other marker owner";
                action = UiAction::ActivateTimecode {
                    media_id: MediaId::new(SourceKind::YouTube, "lmnopqrstuv"),
                    seconds: 95,
                };
            }
            "generic marker" => action = UiAction::SeekPercent(25.0),
            "cancelled" => view.playback_starting = false,
            "cleared marker" => view.loading_chapter_seek = None,
            "live" => view.playback.live = true,
            "at end" => view.loading_chapter_seek.as_mut().unwrap().1 = Duration::from_secs(180),
            "active owner" => view.playing_media_id = Some(media_id),
            _ => unreachable!("all invalidation cases are listed above"),
        }
        let hit_map = HitMap {
            seek_bar: Rect::new(10, 20, 101, 1),
            seek_markers: vec![(action, Rect::new(35, 19, 1, 1))],
            ..HitMap::default()
        };
        for (column, row) in [(35, 19), (60, 20)] {
            if !view.playback.idle && row == 20 {
                continue;
            }
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                None,
                "{invalidation}: {column}, {row}",
            );
        }
    }
}
