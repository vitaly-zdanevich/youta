//! Offline full-frame TestBackend baseline with a large list and long description.

use ratatui::backend::TestBackend;

use super::*;
use crate::test_support::performance::{Fixture, write_probe};

/// Times production frame rendering, not terminal I/O, controller work, or images.
///
/// Alternating the scroll position exercises wrapping and frame diffs rather
/// than repeatedly measuring an unchanged screen. Fixture construction is excluded.
#[test]
#[ignore = "report-only release probe; run through scripts/performance.py instructions"]
fn performance_fixture_probe() {
    let fixture = Fixture::load();
    let mut view = ViewModel {
        rows: (0..fixture.rows)
            .map(|index| RowView {
                title: format!("Offline fixture {index:05}"),
                subtitle: "Fixture creator".to_owned(),
                ..RowView::default()
            })
            .collect(),
        details: Some(DetailView {
            title: "Offline description fixture".to_owned(),
            description: fixture.description(),
            ..DetailView::default()
        }),
        details_focused: true,
        ..ViewModel::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(
        fixture.terminal_columns,
        fixture.terminal_rows,
    ))
    .expect("performance test terminal");
    let settings = UiSettings::default();
    let mut hit_map = HitMap::default();
    let samples = fixture.measure(fixture.render_iterations, || {
        view.details_scroll = if view.details_scroll == 0 { 3 } else { 0 };
        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("performance frame");
        std::hint::black_box(terminal.backend().buffer());
    });
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(text.contains("Offline fixture"));
    assert!(text.contains("Paragraph"));
    write_probe(
        "description_render",
        "test-backend-only",
        fixture.render_iterations,
        view.details
            .as_ref()
            .expect("fixture details")
            .description
            .len(),
        samples,
    );
}
