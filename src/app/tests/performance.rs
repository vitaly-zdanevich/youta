//! Offline controller navigation baseline, separate from process and render costs.

use super::*;
use crate::test_support::performance::{Fixture, write_probe};

/// Exercises real selection dispatch and preliminary details over 10,000 results.
///
/// Fixture generation, row materialization, persistence setup, and shutdown are
/// outside the measured interval. No provider or playback backend is installed.
#[test]
#[ignore = "report-only release probe; run through scripts/performance.py instructions"]
fn performance_fixture_probe() {
    let fixture = Fixture::load();
    let temporary = crate::test_support::canonical_tempdir("performance navigation");
    let config = Config::for_dir(temporary.path().join("config"));
    let store = StateStore::open_in_memory().expect("in-memory performance state");
    let mut controller = AppController::new(config, store, None, None);
    controller.youtube_results = (0..fixture.rows)
        .map(|index| {
            let mut video = subscription_video_summary();
            video.video_id = format!("p{index:010}");
            video.title = format!("Offline fixture {index:05}");
            video.published_at = None;
            SearchItem::Video(video)
        })
        .collect();
    controller.refresh_youtube_rows();
    assert_eq!(controller.view.rows.len(), fixture.rows);
    let mut direction = 1;
    let samples = fixture.measure(fixture.navigation_iterations, || {
        if controller.view.selected == fixture.rows - 1 {
            direction = -1;
        } else if controller.view.selected == 0 {
            direction = 1;
        }
        controller.dispatch(UiAction::MoveSelection(direction));
        std::hint::black_box(controller.view());
    });
    assert!(controller.view.selected > 0);
    assert!(controller.view.details.is_some());
    controller.shutdown();
    write_probe(
        "navigation",
        "controller-only",
        fixture.navigation_iterations,
        subscription_video_summary().description.len(),
        samples,
    );
}
