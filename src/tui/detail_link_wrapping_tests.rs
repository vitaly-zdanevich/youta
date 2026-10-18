//! External labelled links retain complete, copyable URLs on their own rows.

use super::*;
use ratatui::backend::TestBackend;

const RECORDING_LABEL: &str = "MusicBrainz recording 1 · 98.6% match";
const RECORDING_URL: &str =
    "https://musicbrainz.org/recording/2e0fb901-b79c-4230-9b7c-9f0981fbb1e0";

/// Uses a validated-length recording URL without a resolver or browser process.
fn external_link_view() -> ViewModel {
    ViewModel {
        external_opener_available: true,
        details: Some(DetailView {
            links: vec![DetailLinkView {
                label: RECORDING_LABEL.to_owned(),
                url: RECORDING_URL.to_owned(),
                ..DetailLinkView::default()
            }],
            ..DetailView::default()
        }),
        ..ViewModel::default()
    }
}

/// Exercises the real Details renderer without unrelated application panes.
fn draw_links(view: &ViewModel, width: u16, height: u16) -> (Terminal<TestBackend>, HitMap) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut hits = HitMap::default();
    terminal
        .draw(|frame| {
            render_details(
                frame,
                frame.area(),
                view,
                false,
                0,
                &Theme::new(false),
                &mut hits,
                None,
            );
        })
        .unwrap();
    (terminal, hits)
}

/// Reads only painted cells so missing URL suffixes cannot hide in the view model.
fn painted_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// Label and URL have separate rows even when the former combined layout would fit.
#[test]
fn external_labelled_url_always_appears_below_its_label() {
    for width in [120, 90] {
        let view = external_link_view();
        let (terminal, hits) = draw_links(&view, width, 12);
        let rows = painted_rows(&terminal);
        let label_row = rows
            .iter()
            .position(|row| row == RECORDING_LABEL)
            .unwrap_or_else(|| panic!("missing standalone label at width {width}: {rows:?}"));
        assert_eq!(rows[label_row + 1], RECORDING_URL);
        assert_eq!(hits.detail_links.len(), 2);
        assert!(hits.detail_links.iter().all(|(index, _)| *index == 0));
        assert_eq!(view.details.as_ref().unwrap().links.len(), 1);
    }
}

/// Narrow terminals paint the entire target and every fragment opens the original link.
#[test]
fn external_labelled_url_wraps_without_losing_its_target_or_click_identity() {
    for opener in [false, true] {
        let mut view = external_link_view();
        view.external_opener_available = opener;
        let (terminal, hits) = draw_links(&view, 30, 14);
        let rows = painted_rows(&terminal);
        let first_url_row = rows
            .iter()
            .position(|row| row.starts_with("https://"))
            .unwrap_or_else(|| panic!("missing URL below wrapped label: {rows:?}"));
        let expected = wrap_text_lines(RECORDING_URL, 30);
        let expected_label = wrap_text_lines(RECORDING_LABEL, 30);
        assert_eq!(
            &rows[first_url_row - expected_label.len()..first_url_row],
            expected_label
        );
        assert_eq!(
            &rows[first_url_row..first_url_row + expected.len()],
            expected
        );
        assert_eq!(
            rows[first_url_row..first_url_row + expected.len()].concat(),
            RECORDING_URL
        );
        if !opener {
            assert!(hits.detail_links.is_empty());
            continue;
        }
        for (offset, fragment) in expected.iter().enumerate() {
            let row = u16::try_from(first_url_row + offset).unwrap();
            let (index, area) = hits
                .detail_links
                .iter()
                .find(|(_, area)| area.y == row)
                .expect("each URL continuation has its own hit area");
            assert_eq!(*index, 0);
            assert_eq!(area.width, terminal_text_width(fragment));
            for column in [area.x, area.right() - 1] {
                assert_eq!(
                    mouse_action(
                        MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column,
                            row,
                            modifiers: KeyModifiers::NONE,
                        },
                        &hits,
                        &view,
                    ),
                    Some(UiAction::ActivateDetailLink(0))
                );
            }
        }
        assert!(hits.detail_links.iter().all(|(index, _)| *index == 0));
        assert_eq!(view.details.as_ref().unwrap().links[0].url, RECORDING_URL);
    }
}

/// TUI text selection rejoins soft URL wraps instead of copying invalid embedded newlines.
#[test]
fn copying_a_wrapped_external_url_preserves_the_original_target() {
    let view = external_link_view();
    let (_, hits) = draw_links(&view, 30, 14);
    let first = hits
        .detail_text_rows
        .iter()
        .position(|row| row.cells.concat().starts_with("https://"))
        .expect("selectable URL start");
    let last = first + wrap_text_lines(RECORDING_URL, 30).len() - 1;
    let selection = DetailsTextSelection {
        dragging: false,
        anchor: DetailsTextPosition {
            row: first,
            column: 0,
        },
        focus: DetailsTextPosition {
            row: last,
            column: hits.detail_text_rows[last].cells.len() - 1,
        },
    };
    assert_eq!(
        hits.selected_details_text(selection),
        RECORDING_URL,
        "copying a URL across physical rows must preserve one usable URL"
    );
    let label = hits
        .detail_text_rows
        .iter()
        .position(|row| row.cells.concat().starts_with("MusicBrainz"))
        .expect("selectable label start");
    assert_eq!(
        hits.selected_details_text(DetailsTextSelection {
            anchor: DetailsTextPosition {
                row: label,
                column: 0
            },
            ..selection
        }),
        format!("{RECORDING_LABEL}\n{RECORDING_URL}"),
        "the label and URL remain separate source lines"
    );
}

/// Soft wrapping preserves boundary spaces while explicit source line breaks remain real.
#[test]
fn copying_a_wrapped_external_label_preserves_source_whitespace() {
    let mut view = external_link_view();
    let link = &mut view.details.as_mut().unwrap().links[0];
    link.prefix = "By ".to_owned();
    link.label = "Title one rest\nSecond line".to_owned();
    let original = format!("{}{}", link.prefix, link.label);
    let expected_rows = wrap_text_lines(&original, 13);
    assert_eq!(expected_rows[0], "By Title one ");
    let (_, hits) = draw_links(&view, 13, 20);
    let first = hits
        .detail_text_rows
        .iter()
        .position(|row| row.cells.concat().starts_with("By Title one"))
        .expect("selectable wrapped label");
    let last = first + expected_rows.len() - 1;
    let selection = DetailsTextSelection {
        dragging: false,
        anchor: DetailsTextPosition {
            row: first,
            column: 0,
        },
        focus: DetailsTextPosition {
            row: last,
            column: hits.detail_text_rows[last].cells.len() - 1,
        },
    };
    assert_eq!(
        hits.selected_details_text(selection),
        original,
        "copy must retain a real space at a soft wrap and the original explicit newline"
    );
}

/// Original UTF-8 field offsets remain valid after prefix and label share wrapped rows.
#[test]
fn external_labelled_url_preserves_wrapped_unicode_prefix_and_field_highlights() {
    use crate::view::{DetailHighlightRange, DetailHighlightView};

    let prefix = "аўтар: доўгі подпіс ";
    let label = "Запіс · 98.6%";
    let combined = format!("{prefix}{label}");
    let mut view = external_link_view();
    let details = view.details.as_mut().unwrap();
    details.links[0].prefix = prefix.to_owned();
    details.links[0].label = label.to_owned();
    details.search_highlights = [
        (DetailHighlightField::LinkPrefix(0), prefix, "доўгі"),
        (DetailHighlightField::LinkLabel(0), label, "Запіс"),
        (DetailHighlightField::LinkUrl(0), RECORDING_URL, "recording"),
    ]
    .into_iter()
    .map(|(field, source, query)| {
        let start_byte = source.find(query).unwrap();
        DetailHighlightView {
            field,
            ranges: vec![DetailHighlightRange {
                start_byte,
                end_byte: start_byte + query.len(),
            }],
        }
    })
    .collect();
    let (terminal, hits) = draw_links(&view, 12, 18);
    let rows = painted_rows(&terminal);
    let label_lines = wrap_description_source(&combined, 12, &[], &[]);
    let first_row = rows
        .iter()
        .position(|row| row == &combined[label_lines[0].start_byte..label_lines[0].end_byte])
        .unwrap_or_else(|| panic!("missing wrapped Unicode prefix: {rows:?}"));
    let buffer = terminal.backend().buffer();
    let details = view.details.as_ref().unwrap();
    for (offset, line) in label_lines.iter().enumerate() {
        let y = u16::try_from(first_row + offset).unwrap();
        let source = &combined[line.start_byte..line.end_byte];
        assert_eq!(rows[usize::from(y)], source.trim_end());
        let prefix_end = line.end_byte.min(prefix.len());
        let prefix_start = line.start_byte.min(prefix_end);
        let prefix_width = terminal_text_width(&prefix[prefix_start..prefix_end]);
        let row_target = hits.detail_links.iter().find(|(_, area)| area.y == y);
        if line.end_byte <= prefix.len() {
            assert!(row_target.is_none(), "prefix-only rows are not clickable");
        } else {
            let (index, area) = row_target.expect("wrapped label hit target");
            assert_eq!(*index, 0);
            assert_eq!(area.x, prefix_width);
            assert_eq!(area.width, terminal_text_width(source) - prefix_width);
        }
        if prefix_width > 0 {
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: prefix_width - 1,
                        row: y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hits,
                    &view,
                ),
                Some(UiAction::SetDetailsFocus(true))
            );
        }
        let mut x = 0;
        for (offset, character) in source.char_indices() {
            let absolute = line.start_byte + offset;
            let (field, source_byte) = if absolute < prefix.len() {
                (DetailHighlightField::LinkPrefix(0), absolute)
            } else {
                (DetailHighlightField::LinkLabel(0), absolute - prefix.len())
            };
            let highlighted = details.search_highlights.iter().any(|group| {
                group.field == field
                    && group.ranges.iter().any(|range| {
                        range.start_byte <= source_byte && source_byte < range.end_byte
                    })
            });
            assert_eq!(buffer[(x, y)].bg == Color::Yellow, highlighted);
            x += terminal_text_width(&character.to_string());
        }
    }
    let url_lines = wrap_description_source(RECORDING_URL, 12, &[], &[]);
    let first_url_row = first_row + label_lines.len();
    assert_eq!(
        rows[first_url_row..first_url_row + url_lines.len()].concat(),
        RECORDING_URL
    );
    let highlighted_url = &details.search_highlights[2].ranges[0];
    for (offset, line) in url_lines.iter().enumerate() {
        let y = u16::try_from(first_url_row + offset).unwrap();
        for source_byte in line.start_byte..line.end_byte {
            let x = u16::try_from(source_byte - line.start_byte).unwrap();
            assert_eq!(
                buffer[(x, y)].bg == Color::Yellow,
                highlighted_url.start_byte <= source_byte && source_byte < highlighted_url.end_byte
            );
        }
    }
}

/// Keyboard selection reveals the final URL row as well as the selected label.
#[test]
fn external_labelled_url_selection_reveals_all_rows_of_a_later_link() {
    let mut view = external_link_view();
    let details = view.details.as_mut().unwrap();
    let recording = details.links.pop().unwrap();
    details.links = (0..5)
        .map(|index| DetailLinkView {
            label: format!("Earlier link {index}"),
            url: format!("https://example.com/{index}"),
            ..DetailLinkView::default()
        })
        .collect();
    details.links.push(recording);
    view.selected_detail_link = Some(5);
    let (terminal, hits) = draw_links(&view, 90, 4);
    let rows = painted_rows(&terminal);
    let label_row = rows
        .iter()
        .position(|row| row == RECORDING_LABEL)
        .unwrap_or_else(|| panic!("selected label must be visible: {rows:?}"));
    assert_eq!(
        rows.get(label_row + 1).map(String::as_str),
        Some(RECORDING_URL)
    );
    assert_eq!(
        hits.detail_links
            .iter()
            .filter(|(index, _)| *index == 5)
            .count(),
        2
    );
}

/// A loading cover consumes its entire reserved area without fetching an image.
#[derive(Default)]
struct LoadingCover {
    areas: Vec<Rect>,
}

impl ThumbnailRenderer for LoadingCover {
    fn poll(&mut self) -> bool {
        false
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn synchronize(&mut self, _source: Option<&url::Url>, area: Rect) -> bool {
        self.areas.push(area);
        true
    }

    fn clear(&mut self) -> bool {
        false
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        frame.render_widget(Block::default().style(theme.muted), area);
    }
}

/// Native VK covers reserve the expanded label/URL height before allocating artwork.
#[test]
fn external_labelled_url_remains_complete_below_a_vk_loading_cover() {
    for width in [90, 30] {
        let mut view = external_link_view();
        view.screen = Screen::Vk;
        let details = view.details.as_mut().unwrap();
        details.media_id = Some(MediaId::new(SourceKind::Vk, "7_11"));
        details.thumbnail_url = Some(url::Url::parse("https://images.example/cover.jpg").unwrap());
        details.thumbnail_dimensions = Some((600, 600));
        let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
        let mut hits = HitMap::default();
        let mut cover = LoadingCover::default();
        terminal
            .draw(|frame| {
                render_details_with_terminal_window(
                    frame,
                    frame.area(),
                    &view,
                    false,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    TerminalWindowMetrics::new(width, 18, width * 10, 360),
                    &Theme::new(false),
                    &mut hits,
                    Some(&mut cover),
                );
            })
            .unwrap();
        let rows = painted_rows(&terminal);
        let first_url_row = rows
            .iter()
            .position(|row| row.starts_with("https://"))
            .unwrap_or_else(|| panic!("cover must leave the complete URL visible: {rows:?}"));
        let expected = wrap_text_lines(RECORDING_URL, width);
        assert_eq!(
            rows.get(first_url_row..first_url_row + expected.len()),
            Some(expected.as_slice())
        );
        assert!(!cover.areas.is_empty());
        let first_url_row = u16::try_from(first_url_row).unwrap();
        assert!(
            cover
                .areas
                .iter()
                .all(|area| area.bottom() <= first_url_row)
        );
    }
}
