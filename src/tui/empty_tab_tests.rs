//! Empty search tabs retire the previous pane's physical cells and artwork.

use super::*;
use ratatui::backend::{Backend, TestBackend};

/// Recreates a paused YouTube item before switching to an untouched VK tab.
fn youtube_details_view() -> ViewModel {
    let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
    ViewModel {
        screen: Screen::Search,
        playing_media_id: Some(media_id.clone()),
        playback: PlaybackStatus {
            idle: false,
            paused: true,
            position: Duration::from_secs(95),
            duration: Some(Duration::from_secs(180)),
            ..PlaybackStatus::default()
        },
        details: Some(DetailView {
            media_id: Some(media_id),
            title: "Fixture video".to_owned(),
            source: "YouTube".to_owned(),
            description: "A short description with an emoji 🔴".to_owned(),
            thumbnail_url: Some(url::Url::parse("https://example.invalid/cover.jpg").unwrap()),
            ..DetailView::default()
        }),
        ..ViewModel::default()
    }
}

/// Model terminal/Ratatui width disagreement without modifying the prior logical frame.
#[test]
fn entering_empty_vk_clears_untracked_description_cells_once() {
    for (width, height) in [(120, 32), (60, 32)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut view = youtube_details_view();
        let mut hits = HitMap::default();
        let settings = UiSettings::default();
        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hits))
            .unwrap();
        // Retire the first owner's one-shot invalidation before injecting
        // terminal-only drift; otherwise its previous AlwaysUpdate cells
        // would erase the fixture during the next unrelated transition.
        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hits))
            .unwrap();
        let panel = hits.details_panel;
        assert!(hits.information_panel_identity.is_some());
        let stale = {
            let buffer = terminal.backend().buffer();
            (panel.top()..panel.bottom())
                .flat_map(|y| (panel.left()..panel.right()).rev().map(move |x| (x, y)))
                .filter(|position| buffer[*position].symbol() == " ")
                .take(7)
                .collect::<Vec<_>>()
        };
        assert_eq!(stale.len(), 7);
        for ((x, y), symbol) in stale
            .iter()
            .copied()
            .zip(["е", "е", "я", "я", "t", ":", "n"])
        {
            let mut cell = ratatui::buffer::Cell::default();
            cell.set_symbol(symbol);
            terminal
                .backend_mut()
                .draw([(x, y, &cell)].into_iter())
                .unwrap();
        }
        view.screen = Screen::Vk;
        view.details = None;
        assert!(empty_search_prompt_visible(&view));
        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hits))
            .unwrap();
        for (x, y) in stale {
            assert_eq!(
                terminal.backend().buffer()[(x, y)].symbol(),
                " ",
                "{width}x{height}: empty VK must erase physical leftovers at ({x}, {y})"
            );
        }
        assert!(hits.information_panel_identity.is_none());
        assert!(hits.thumbnail_area.is_none());
        assert!(hits.detail_links.is_empty());
        let unchanged = terminal
            .draw(|frame| render(frame, &view, &settings, &mut hits))
            .unwrap();
        assert!(
            unchanged
                .buffer
                .content
                .iter()
                .all(|cell| { cell.diff_option != CellDiffOption::AlwaysUpdate }),
            "an unchanged empty tab must not force full-pane rewrites"
        );
    }
}

/// Records artwork ownership without downloads or a terminal image protocol.
#[derive(Default)]
struct ArtworkRecorder {
    clears: usize,
    renders: usize,
}

impl ThumbnailRenderer for ArtworkRecorder {
    fn poll(&mut self) -> bool {
        false
    }
    fn is_enabled(&self) -> bool {
        true
    }
    fn synchronize(&mut self, _source: Option<&url::Url>, _area: Rect) -> bool {
        false
    }
    fn clear(&mut self) -> bool {
        self.clears += 1;
        true
    }
    fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect, _theme: &Theme) {
        self.renders += 1;
    }
}

/// Leaving selected artwork for an empty tab releases its previous renderer target.
#[test]
fn entering_empty_vk_retires_the_previous_artwork_target() {
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    let mut view = youtube_details_view();
    let mut hits = HitMap::default();
    let mut artwork = ArtworkRecorder::default();
    let settings = UiSettings::default();
    terminal
        .draw(|frame| {
            render_frame(frame, &view, &settings, &mut hits, Some(&mut artwork));
        })
        .unwrap();
    assert!(artwork.renders > 0);
    let before = artwork.clears;
    view.screen = Screen::Vk;
    view.details = None;
    terminal
        .draw(|frame| {
            render_frame(frame, &view, &settings, &mut hits, Some(&mut artwork));
        })
        .unwrap();
    assert!(
        artwork.clears > before,
        "an empty tab must retire the old artwork target"
    );
}
