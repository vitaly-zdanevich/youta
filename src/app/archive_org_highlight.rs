//! Literal Archive search highlighting, computed outside the renderer hot path.

use crate::view::{DetailHighlightField, DetailHighlightRange, DetailHighlightView, DetailView};
use regex::{Regex, RegexBuilder};
use unicode_segmentation::UnicodeSegmentation;

/// Owns only the current submitted query and selected Details projection.
#[derive(Default)]
pub(super) struct ArchiveSearchHighlighter {
    query: String,
    expression: Option<Regex>,
    text: Vec<(DetailHighlightField, String)>,
    highlights: Vec<DetailHighlightView>,
    #[cfg(test)]
    compilations: usize,
    #[cfg(test)]
    scans: usize,
}

impl ArchiveSearchHighlighter {
    /// Projects search styling without changing provider text or interactive spans.
    pub(super) fn apply(&mut self, query: &str, details: &mut DetailView) {
        let query = query.trim();
        if self.query != query {
            query.clone_into(&mut self.query);
            self.text.clear();
            self.highlights.clear();
            self.expression =
                if query.is_empty() || query.len() > 512 || query.chars().any(char::is_control) {
                    None
                } else {
                    #[cfg(test)]
                    {
                        self.compilations += 1;
                    }
                    RegexBuilder::new(&regex::escape(query))
                        .case_insensitive(true)
                        .size_limit(512 * 1024)
                        .dfa_size_limit(128 * 1024)
                        .build()
                        .ok()
                };
        }
        let Some(expression) = &self.expression else {
            details.search_highlights.clear();
            return;
        };
        let text = fields(details);
        if !self
            .text
            .iter()
            .map(|(field, value)| (field, value.as_str()))
            .eq(text.iter().map(|(field, value)| (field, *value)))
        {
            #[cfg(test)]
            {
                self.scans += 1;
            }
            self.highlights = text
                .iter()
                .filter_map(|(field, value)| {
                    let ranges = grapheme_matches(expression, value);
                    (!ranges.is_empty()).then(|| DetailHighlightView {
                        field: *field,
                        ranges,
                    })
                })
                .collect();
            self.text = text
                .into_iter()
                .map(|(field, value)| (field, value.to_owned()))
                .collect();
        }
        details.search_highlights.clone_from(&self.highlights);
    }
}

/// Expands styling to complete graphemes, walking source boundaries once.
///
/// Byte-correct regex matches may still split accents or emoji sequences. Both
/// frontends receive the same merged ranges, cached with the Details projection;
/// no segmentation runs in the renderer or adds padding to the original text.
fn grapheme_matches(expression: &Regex, value: &str) -> Vec<DetailHighlightRange> {
    let mut graphemes = value
        .grapheme_indices(true)
        .map(|(start, text)| start..start + text.len())
        .peekable();
    let mut ranges: Vec<DetailHighlightRange> = Vec::new();
    for matched in expression.find_iter(value) {
        while graphemes
            .peek()
            .is_some_and(|range| range.end <= matched.start())
        {
            graphemes.next();
        }
        let start_byte = graphemes
            .peek()
            .map_or(matched.start(), |range| range.start);
        while graphemes
            .peek()
            .is_some_and(|range| range.end < matched.end())
        {
            graphemes.next();
        }
        let end_byte = graphemes.peek().map_or(matched.end(), |range| range.end);
        if let Some(previous) = ranges
            .last_mut()
            .filter(|previous| previous.end_byte >= start_byte)
        {
            previous.end_byte = previous.end_byte.max(end_byte);
        } else {
            ranges.push(DetailHighlightRange {
                start_byte,
                end_byte,
            });
        }
    }
    ranges
}

/// Uses provider-bounded text, retaining one projection rather than a per-item cache.
fn fields(details: &DetailView) -> Vec<(DetailHighlightField, &str)> {
    use DetailHighlightField::{
        ChannelName, Comments, Description, Length, License, Likes, LinkLabel, LinkPrefix, LinkUrl,
        Published, Source, Title, Views,
    };
    let mut text = vec![
        (Title, details.title.as_str()),
        (Description, details.description.as_str()),
        (ChannelName, details.channel_name.as_str()),
        (Source, details.source.as_str()),
        (Length, details.length.as_str()),
        (Likes, details.likes.as_str()),
        (Views, details.views.as_str()),
        (Comments, details.comments.as_str()),
        (Published, details.published.as_str()),
        (License, details.license.as_str()),
    ];
    for (index, link) in details.links.iter().enumerate() {
        text.extend([
            (LinkLabel(index), link.label.as_str()),
            (LinkPrefix(index), link.prefix.as_str()),
            (LinkUrl(index), link.url.as_str()),
        ]);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{DetailHighlightRange, DetailLinkView};

    fn ranges(details: &DetailView, field: DetailHighlightField) -> &[DetailHighlightRange] {
        details
            .search_highlights
            .iter()
            .find(|group| group.field == field)
            .map_or(&[], |group| group.ranges.as_slice())
    }

    #[test]
    fn archive_search_highlighting_serializes_original_utf8_offsets_for_both_frontends() {
        let mut details = DetailView {
            title: "😀 Jazz".into(),
            links: vec![DetailLinkView {
                label: "JAZZ".into(),
                ..DetailLinkView::default()
            }],
            ..DetailView::default()
        };
        ArchiveSearchHighlighter::default().apply("jazz", &mut details);
        assert_eq!(
            serde_json::to_value(&details.search_highlights).unwrap(),
            serde_json::json!([
                {"field": "Title", "ranges": [{"start_byte": 5, "end_byte": 9}]},
                {"field": {"LinkLabel": 0}, "ranges": [{"start_byte": 0, "end_byte": 4}]}
            ])
        );
    }

    #[test]
    fn archive_search_highlighting_is_unicode_case_insensitive_and_literal() {
        let mut details = DetailView {
            title: "😀 ПРИВЕТ привет ПрИвЕт".into(),
            description: "Literal a.*[b] and A.*[B]; not axxb".into(),
            ..DetailView::default()
        };
        let original = details.description.clone();
        let mut highlighter = ArchiveSearchHighlighter::default();
        highlighter.apply("привет", &mut details);
        let title_ranges = ranges(&details, DetailHighlightField::Title);
        assert_eq!(title_ranges.len(), 3);
        assert_eq!(title_ranges[0].start_byte, "😀 ".len());
        for span in title_ranges {
            assert_eq!(
                details.title[span.start_byte..span.end_byte].to_lowercase(),
                "привет"
            );
        }
        highlighter.apply("a.*[b]", &mut details);
        assert!(ranges(&details, DetailHighlightField::Title).is_empty());
        assert_eq!(ranges(&details, DetailHighlightField::Description).len(), 2);
        assert_eq!(details.description, original);
    }

    #[test]
    fn archive_search_highlighting_includes_metadata_and_long_description_tail() {
        let mut details = DetailView {
            description: format!(
                "Creator: Jazz\nTopics: JAZZ, live\n\n{}jAzZ",
                "x".repeat(60_000)
            ),
            published: "2026 September 13".into(),
            links: vec![DetailLinkView {
                prefix: "Jazz collection: ".into(),
                label: "JAZZ archive".into(),
                url: "https://archive.org/details/jazz".into(),
                ..DetailLinkView::default()
            }],
            ..DetailView::default()
        };
        let original = details.clone();
        let mut highlighter = ArchiveSearchHighlighter::default();
        highlighter.apply("jazz", &mut details);
        let matches = ranges(&details, DetailHighlightField::Description);
        assert_eq!(matches.len(), 3);
        assert_eq!(matches.last().unwrap().end_byte, details.description.len());
        for field in [
            DetailHighlightField::LinkLabel(0),
            DetailHighlightField::LinkPrefix(0),
            DetailHighlightField::LinkUrl(0),
        ] {
            assert_eq!(ranges(&details, field).len(), 1);
        }
        highlighter.apply("2026", &mut details);
        assert_eq!(ranges(&details, DetailHighlightField::Published).len(), 1);
        highlighter.apply("  ", &mut details);
        assert_eq!(
            details, original,
            "clearing the query restores the exact unstyled projection"
        );
    }

    #[test]
    fn archive_search_highlighting_reuses_the_query_and_unchanged_text() {
        let mut details = DetailView {
            description: "Jazz".into(),
            ..DetailView::default()
        };
        let mut highlighter = ArchiveSearchHighlighter::default();
        highlighter.apply("jazz", &mut details);
        for _ in 0..20 {
            highlighter.apply(" jazz ", &mut details);
        }
        assert_eq!((highlighter.compilations, highlighter.scans), (1, 1));
        details.description.push_str(" JAZZ");
        highlighter.apply("jazz", &mut details);
        assert_eq!((highlighter.compilations, highlighter.scans), (1, 2));
        assert_eq!(ranges(&details, DetailHighlightField::Description).len(), 2);
        highlighter.apply("different", &mut details);
        assert_eq!((highlighter.compilations, highlighter.scans), (2, 3));
        assert!(details.search_highlights.is_empty());
        highlighter.apply(&"x".repeat(513), &mut details);
        assert_eq!(highlighter.compilations, 2);
        assert!(details.search_highlights.is_empty());
    }

    #[test]
    fn archive_search_highlighting_preserves_grapheme_cells_and_merges_component_matches() {
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            style::{Color, Style},
            text::{Line, Span},
            widgets::{Paragraph, Widget},
        };
        for (query, grapheme) in [("e", "e\u{301}"), ("❤", "❤️"), ("👩", "👩‍👩‍👧‍👧")]
        {
            let source = format!("before {grapheme} after");
            let mut details = DetailView {
                description: source.clone(),
                ..DetailView::default()
            };
            ArchiveSearchHighlighter::default().apply(query, &mut details);
            let matches = ranges(&details, DetailHighlightField::Description);
            let area = Rect::new(0, 0, 50, 1);
            let mut original = Buffer::empty(area);
            Paragraph::new(source.as_str()).render(area, &mut original);
            let mut spans = Vec::new();
            let mut cursor = 0;
            for matched in matches {
                spans.push(Span::raw(&source[cursor..matched.start_byte]));
                spans.push(Span::styled(
                    &source[matched.start_byte..matched.end_byte],
                    Style::default().bg(Color::Yellow),
                ));
                cursor = matched.end_byte;
            }
            spans.push(Span::raw(&source[cursor..]));
            let mut highlighted = Buffer::empty(area);
            Paragraph::new(Line::from(spans)).render(area, &mut highlighted);
            assert_eq!(
                highlighted
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<Vec<_>>(),
                original
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<Vec<_>>(),
                "query {query} must not change the rendered cells"
            );
            let prefix = "before ".len();
            assert!(matches.contains(&DetailHighlightRange {
                start_byte: prefix,
                end_byte: prefix + grapheme.len()
            }));
            assert!(
                matches
                    .windows(2)
                    .all(|pair| pair[0].end_byte <= pair[1].start_byte)
            );
        }
    }
}
