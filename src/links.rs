//! Selectable links extracted from plain-text media descriptions.
//!
//! Parsing is intentionally dependency-light and operates on byte ranges so a
//! terminal renderer can style and hit-test links without rewriting the source
//! description.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::providers::validate_youtube_video_id;

/// The form used by a `YouTube` channel URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelReferenceKind {
    /// Stable `/channel/UC…` identifier.
    Id,
    /// Modern `/@handle` reference.
    Handle,
    /// Legacy `/user/name` reference.
    User,
    /// Legacy `/c/name` custom path.
    Custom,
}

/// A navigation target found in a description.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkTarget {
    /// A `YouTube` video and an optional initial seek position.
    YouTubeVideo {
        /// Stable eleven-character video identifier.
        video_id: String,
        /// Initial playback position encoded in the URL.
        start_seconds: Option<u64>,
    },
    /// A `YouTube` channel reference.
    YouTubeChannel {
        /// URL form used for the channel.
        reference_kind: ChannelReferenceKind,
        /// Channel identifier, handle, user name, or custom name.
        reference: String,
    },
    /// A `YouTube` playlist.
    YouTubePlaylist {
        /// Stable playlist identifier.
        playlist_id: String,
    },
    /// A timecode that seeks within the currently selected media.
    Timecode {
        /// Seek target in seconds.
        seconds: u64,
    },
    /// A hashtag that opens an internal hashtag search.
    Hashtag {
        /// Tag text without the leading hash mark.
        tag: String,
    },
}

impl LinkTarget {
    /// Builds a canonical web URL when the target is independently addressable.
    ///
    /// Standalone timecodes return `None` because they require the current media
    /// identifier and should be handled by the player navigation stack.
    #[must_use]
    pub fn canonical_url(&self) -> Option<Url> {
        let mut url = Url::parse("https://www.youtube.com/").ok()?;
        match self {
            Self::YouTubeVideo {
                video_id,
                start_seconds,
            } => {
                url.set_path("watch");
                {
                    let mut query = url.query_pairs_mut();
                    query.append_pair("v", video_id);
                    if let Some(seconds) = start_seconds {
                        query.append_pair("t", &seconds.to_string());
                    }
                }
            }
            Self::YouTubeChannel {
                reference_kind,
                reference,
            } => {
                let mut segments = url.path_segments_mut().ok()?;
                match reference_kind {
                    ChannelReferenceKind::Id => {
                        segments.push("channel");
                        segments.push(reference);
                    }
                    ChannelReferenceKind::Handle => {
                        segments.push(&format!("@{reference}"));
                    }
                    ChannelReferenceKind::User => {
                        segments.push("user");
                        segments.push(reference);
                    }
                    ChannelReferenceKind::Custom => {
                        segments.push("c");
                        segments.push(reference);
                    }
                }
            }
            Self::YouTubePlaylist { playlist_id } => {
                url.set_path("playlist");
                url.query_pairs_mut().append_pair("list", playlist_id);
            }
            Self::Hashtag { tag } => {
                let mut segments = url.path_segments_mut().ok()?;
                segments.push("hashtag");
                segments.push(tag);
            }
            Self::Timecode { .. } => return None,
        }
        Some(url)
    }
}

/// A selectable description span.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DescriptionLink {
    /// Inclusive UTF-8 byte offset into the original description.
    pub start_byte: usize,
    /// Exclusive UTF-8 byte offset into the original description.
    pub end_byte: usize,
    /// Parsed navigation action.
    pub target: LinkTarget,
}

impl DescriptionLink {
    /// Returns the exact linked text when the description still matches.
    #[must_use]
    pub fn selected_text<'a>(&self, description: &'a str) -> Option<&'a str> {
        description.get(self.start_byte..self.end_byte)
    }
}

/// A `YouTube` video URL that may be opened inside the Details panel.
///
/// The byte range identifies the original URL. Renderers place their compact
/// action immediately after `end_byte` without rewriting the description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DescriptionVideoLink {
    /// Inclusive UTF-8 byte offset of the original URL.
    pub start_byte: usize,
    /// Exclusive UTF-8 byte offset of the original URL.
    pub end_byte: usize,
    /// Stable eleven-character `YouTube` video identifier.
    pub video_id: String,
    /// Optional initial position encoded in the URL.
    pub start_seconds: Option<u64>,
}

/// A chapter inferred from a line-leading timecode in a media description.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DescriptionChapter {
    /// Chapter title following the timestamp and optional dash separator.
    pub title: String,
    /// Inclusive chapter start in seconds.
    pub start_seconds: u64,
    /// Exclusive chapter end in seconds, when another chapter or duration provides it.
    pub end_seconds: Option<u64>,
    /// Inclusive UTF-8 byte offset of the timestamp in the original description.
    pub timestamp_start_byte: usize,
    /// Exclusive UTF-8 byte offset of the timestamp in the original description.
    pub timestamp_end_byte: usize,
}

impl DescriptionChapter {
    /// Returns the exact timestamp text when the description still matches.
    #[must_use]
    pub fn timestamp_text<'a>(&self, description: &'a str) -> Option<&'a str> {
        description.get(self.timestamp_start_byte..self.timestamp_end_byte)
    }
}

/// Extracts supported URLs, standalone timecodes, and hashtags.
///
/// Results are returned in display order and never overlap. Unsupported and
/// malformed URLs remain ordinary description text.
#[must_use]
pub fn parse_description_links(description: &str) -> Vec<DescriptionLink> {
    let mut links = parse_url_links(description);
    parse_timecodes(description, &mut links);
    parse_hashtags(description, &mut links);
    links.sort_unstable_by_key(|link| (link.start_byte, link.end_byte));
    links
}

/// Extracts only `YouTube` video URLs eligible for internal Details navigation.
///
/// Channel and playlist URLs intentionally remain ordinary text because their
/// internal screens have different state and provider requirements.
#[must_use]
pub fn parse_description_video_links(description: &str) -> Vec<DescriptionVideoLink> {
    parse_description_links(description)
        .into_iter()
        .filter_map(|link| match link.target {
            LinkTarget::YouTubeVideo {
                video_id,
                start_seconds,
            } => Some(DescriptionVideoLink {
                start_byte: link.start_byte,
                end_byte: link.end_byte,
                video_id,
                start_seconds,
            }),
            _ => None,
        })
        .collect()
}

/// Maps readable URL graphemes to their unchanged source bytes for display only.
///
/// Only printable, percent-encoded non-ASCII UTF-8 outside the authority is
/// decoded, exactly once. ASCII escapes retain their delimiter/whitespace
/// spelling. Opening, copying, and action parsing use the original text.
/// Fields above 256 KiB or more than 4,096 replacements fall back completely to
/// their original display; individual URLs above 16 KiB remain unchanged.
#[cfg(feature = "controller")]
#[must_use]
pub fn description_url_escapes(description: &str) -> Vec<crate::view::DetailUrlEscapeView> {
    if description.len() > MAX_URL_DISPLAY_SOURCE_BYTES {
        return Vec::new();
    }
    let mut replacements = Vec::new();
    for (start, end) in description_url_ranges(description) {
        let raw = &description[start..end];
        if raw.len() > 16 * 1024 || !raw.contains('%') {
            continue;
        }
        if append_url_escapes(raw, start, &mut replacements).is_none() {
            return Vec::new();
        }
    }
    replacements
}

/// Caps both display scanning and a caller's optional copy of the current source.
#[cfg(feature = "controller")]
pub(crate) const MAX_URL_DISPLAY_SOURCE_BYTES: usize = 256 * 1024;

/// Builds grapheme mappings from a bounded URL, retaining exact source boundaries.
#[cfg(feature = "controller")]
fn append_url_escapes(
    raw: &str,
    source_offset: usize,
    replacements: &mut Vec<crate::view::DetailUrlEscapeView>,
) -> Option<()> {
    use unicode_segmentation::UnicodeSegmentation;

    let Some(prefix) = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
    else {
        return Some(());
    };
    if Url::parse(raw)
        .ok()
        .is_none_or(|url| url.host_str().is_none())
    {
        return Some(());
    }
    // Percent syntax is validated before decoding any part, including malformed
    // sequences which Url accepts without repairing. Invalid UTF-8 runs stay raw.
    let bytes = raw.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'%' {
            if bytes
                .get(cursor + 1..cursor + 3)
                .is_none_or(|pair| !pair.iter().all(u8::is_ascii_hexdigit))
            {
                return Some(());
            }
            cursor += 3;
        } else {
            cursor += 1;
        }
    }
    let authority_end =
        raw.len() - prefix.len() + prefix.find(['/', '?', '#']).unwrap_or(prefix.len());
    let mut display = String::with_capacity(raw.len());
    let mut boundaries = vec![(0, 0)];
    append_literal_url_text(&raw[..authority_end], 0, &mut display, &mut boundaries);
    cursor = authority_end;
    while cursor < raw.len() {
        if bytes[cursor] != b'%' {
            let character = raw[cursor..].chars().next()?;
            let end = cursor + character.len_utf8();
            append_literal_url_text(&raw[cursor..end], cursor, &mut display, &mut boundaries);
            cursor = end;
            continue;
        }
        let mut end = cursor;
        while bytes.get(end) == Some(&b'%') {
            end += 3;
        }
        let decoded = percent_encoding::percent_decode_str(&raw[cursor..end]).decode_utf8();
        if let Ok(decoded) = decoded {
            for character in decoded.chars() {
                let encoded_end = cursor + character.len_utf8() * 3;
                if readable_url_character(character) {
                    display.push(character);
                    boundaries.push((display.len(), encoded_end));
                } else {
                    append_literal_url_text(
                        &raw[cursor..encoded_end],
                        cursor,
                        &mut display,
                        &mut boundaries,
                    );
                }
                cursor = encoded_end;
            }
        } else {
            append_literal_url_text(&raw[cursor..end], cursor, &mut display, &mut boundaries);
        }
        cursor = end;
    }
    for (start, grapheme) in display.grapheme_indices(true) {
        let end = start + grapheme.len();
        let original_start = boundaries[boundaries
            .binary_search_by_key(&start, |&(byte, _)| byte)
            .ok()?]
        .1;
        let original_end = boundaries[boundaries
            .binary_search_by_key(&end, |&(byte, _)| byte)
            .ok()?]
        .1;
        if original_start < authority_end || grapheme == &raw[original_start..original_end] {
            continue;
        }
        // A combining mark must not decorate a reserved escape's trailing hex
        // digit, disguising its spelling. Keep that entire grapheme unchanged.
        if (original_start >= 1 && bytes[original_start - 1] == b'%')
            || (original_start >= 2 && bytes[original_start - 2] == b'%')
        {
            continue;
        }
        if replacements.len() == 4_096 {
            return None;
        }
        replacements.push(crate::view::DetailUrlEscapeView {
            start_byte: source_offset + original_start,
            end_byte: source_offset + original_end,
            text: grapheme.to_owned(),
        });
    }
    Some(())
}

/// Copies literal URL characters while recording their original UTF-8 endpoints.
#[cfg(feature = "controller")]
fn append_literal_url_text(
    text: &str,
    offset: usize,
    display: &mut String,
    boundaries: &mut Vec<(usize, usize)>,
) {
    for (start, character) in text.char_indices() {
        display.push(character);
        boundaries.push((display.len(), offset + start + character.len_utf8()));
    }
}

/// Excludes invisible controls, directional formatting, and all encoded ASCII.
/// Format ranges follow the Unicode 17.0 character database (`General_Category=Cf`):
/// <https://www.unicode.org/Public/17.0.0/ucd/extracted/DerivedGeneralCategory.txt>.
#[cfg(feature = "controller")]
fn readable_url_character(character: char) -> bool {
    !character.is_ascii()
        && !character.is_control()
        && !character.is_whitespace()
        && !matches!(character,
            '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}'
            | '\u{890}'..='\u{891}' | '\u{8e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}'
            | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{fdd0}'..='\u{fdef}'
            | '\u{110bd}' | '\u{110cd}' | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
        && (u32::from(character) & 0xffff) < 0xfffe
}

/// Infers ordered chapters from line-leading timecodes in a description.
///
/// A timecode may be indented with whitespace or preceded by one common list
/// marker, such as `➤`, `•`, or `-`. Its nonempty title may follow whitespace
/// directly or an optional `-`, `–`, or `—` separator. Inline timecodes,
/// malformed markers, duplicates, decreasing starts, and markers at or beyond
/// a known media duration are ignored. Each chapter ends at the next accepted
/// marker; the final chapter ends at `duration_seconds` when known.
#[must_use]
pub fn parse_description_chapters(
    description: &str,
    duration_seconds: Option<u64>,
) -> Vec<DescriptionChapter> {
    let links = parse_description_links(description);
    let mut chapters = Vec::new();
    let mut previous_start = None;

    for link in links {
        let LinkTarget::Timecode { seconds } = link.target else {
            continue;
        };
        if duration_seconds.is_some_and(|duration| seconds >= duration)
            || previous_start.is_some_and(|previous| seconds <= previous)
        {
            continue;
        }

        let line_start = description[..link.start_byte]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        if !is_supported_chapter_line_prefix(&description[line_start..link.start_byte]) {
            continue;
        }

        let line_end = description[link.end_byte..]
            .find('\n')
            .map_or(description.len(), |offset| link.end_byte + offset);
        let mut title = description[link.end_byte..line_end].trim();
        if let Some(after_separator) = title.strip_prefix(['-', '–', '—']) {
            title = after_separator.trim();
        }
        if title.is_empty() {
            continue;
        }

        previous_start = Some(seconds);
        chapters.push(DescriptionChapter {
            title: title.to_owned(),
            start_seconds: seconds,
            end_seconds: None,
            timestamp_start_byte: link.start_byte,
            timestamp_end_byte: link.end_byte,
        });
    }

    for index in 0..chapters.len() {
        chapters[index].end_seconds = chapters
            .get(index + 1)
            .map(|next| next.start_seconds)
            .or(duration_seconds);
    }
    chapters
}

/// Returns a compact chapter title suitable for a seek bar or chapter line.
///
/// One sentence-ending period is removed, while an ellipsis and all source
/// text outside the returned slice remain unchanged.
#[must_use]
pub fn chapter_title_for_display(title: &str) -> &str {
    let title = title.trim_end();
    if title.ends_with('.') && !title.ends_with("..") {
        title.strip_suffix('.').map_or(title, str::trim_end)
    } else {
        title
    }
}

/// Returns whether a chapter title is the exact Russian advertisement label.
///
/// Surrounding whitespace and one sentence-ending period are ignored in the
/// same way as seek-bar labels. Matching deliberately remains case-sensitive
/// and language-specific so titles such as `Реклама 2`, `РЕКЛАМА`, or
/// `Advertisement` are never skipped accidentally.
#[must_use]
pub fn is_advertisement_chapter_title(title: &str) -> bool {
    chapter_title_for_display(title.trim()) == "Реклама"
}

/// Normalizes recognized chapter lines for display in media Details.
///
/// An optional list marker and indentation are removed, the timestamp remains
/// exact and clickable, and [`chapter_title_for_display`] compacts the title.
/// Ordinary lines, line endings, and inline timecodes are preserved.
#[must_use]
pub fn normalize_description_chapter_lines(description: &str) -> String {
    let chapters = parse_description_chapters(description, None);
    if chapters.is_empty() {
        return description.to_owned();
    }

    let mut normalized = String::with_capacity(description.len());
    let mut copied_until = 0;
    for chapter in chapters {
        let line_start = description[..chapter.timestamp_start_byte]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        if line_start < copied_until {
            continue;
        }
        let line_break = description[chapter.timestamp_end_byte..]
            .find('\n')
            .map_or(description.len(), |offset| {
                chapter.timestamp_end_byte + offset
            });
        let content_end = line_break
            .checked_sub(1)
            .filter(|index| description.as_bytes().get(*index) == Some(&b'\r'))
            .unwrap_or(line_break);
        let Some(timestamp) = chapter.timestamp_text(description) else {
            continue;
        };

        normalized.push_str(&description[copied_until..line_start]);
        normalized.push_str(timestamp);
        let title = chapter_title_for_display(&chapter.title);
        if !title.is_empty() {
            normalized.push(' ');
            normalized.push_str(title);
        }
        copied_until = content_end;
    }
    normalized.push_str(&description[copied_until..]);
    normalized
}

/// Returns whether text before a line's timecode is safe chapter-list syntax.
///
/// Keeping this allowlist narrow prevents ordinary prose containing a
/// timestamp from becoming a seek-bar chapter.
fn is_supported_chapter_line_prefix(prefix: &str) -> bool {
    matches!(
        prefix.trim(),
        "" | "-"
            | "*"
            | "•"
            | "◦"
            | "▪"
            | "▫"
            | "‣"
            | "⁃"
            | "➤"
            | "▶"
            | "►"
            | "→"
            | "➡"
            | "➡️"
    )
}

/// Validates and classifies a `YouTube` URL.
///
/// The parser accepts `youtube.com`, `music.youtube.com`, mobile links,
/// privacy-enhanced embed links, and `youtu.be`. Lookalike domains,
/// credential-bearing URLs, and invalid identifiers are rejected.
#[must_use]
pub fn parse_youtube_url(url: &Url) -> Option<LinkTarget> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if matches!(host.as_str(), "youtu.be" | "www.youtu.be") {
        let video_id = url.path_segments()?.find(|segment| !segment.is_empty())?;
        return video_target(video_id, parse_start_seconds(url));
    }
    if !matches!(
        host.as_str(),
        "youtube.com"
            | "www.youtube.com"
            | "m.youtube.com"
            | "music.youtube.com"
            | "youtube-nocookie.com"
            | "www.youtube-nocookie.com"
    ) {
        return None;
    }

    let segments = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match segments.as_slice() {
        ["watch"] | [] => {
            if let Some(video_id) = query_value(url, "v") {
                return video_target(video_id.as_ref(), parse_start_seconds(url));
            }
            query_value(url, "list")
                .as_deref()
                .and_then(playlist_target)
        }
        ["shorts" | "live" | "embed", video_id, ..] => {
            video_target(video_id, parse_start_seconds(url))
        }
        ["playlist", ..] => query_value(url, "list")
            .as_deref()
            .and_then(playlist_target),
        ["channel", channel_id, ..] => channel_target(ChannelReferenceKind::Id, channel_id),
        ["user", user, ..] => channel_target(ChannelReferenceKind::User, user),
        ["c", custom, ..] => channel_target(ChannelReferenceKind::Custom, custom),
        [first, ..] if first.starts_with('@') => {
            channel_target(ChannelReferenceKind::Handle, &first[1..])
        }
        ["hashtag", tag, ..] => hashtag_target(tag),
        _ => None,
    }
}

fn parse_url_links(description: &str) -> Vec<DescriptionLink> {
    description_url_ranges(description)
        .filter_map(|(start_byte, end_byte)| {
            let raw = &description[start_byte..end_byte];
            let normalized = if raw.starts_with("http://") || raw.starts_with("https://") {
                raw.to_owned()
            } else {
                format!("https://{raw}")
            };
            let target = parse_youtube_url(&Url::parse(&normalized).ok()?)?;
            Some(DescriptionLink {
                start_byte,
                end_byte,
                target,
            })
        })
        .collect()
}

/// Shares existing URL boundaries between navigation and display-only decoding.
fn description_url_ranges(description: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut index = 0;
    std::iter::from_fn(move || {
        while index < description.len() {
            let Some(character) = description[index..].chars().next() else {
                return None;
            };
            let prefix = url_prefix(&description[index..]);
            if prefix.is_none() || !is_url_start_boundary(description, index) {
                index += character.len_utf8();
                continue;
            }

            let mut end = description.len();
            for (offset, candidate) in description[index..].char_indices() {
                if offset > 0 && is_url_terminator(candidate) {
                    end = index + offset;
                    break;
                }
            }
            end = trim_url_end(description, index, end);
            if end <= index {
                index += character.len_utf8();
                continue;
            }

            let start = index;
            index = end.max(index + character.len_utf8());
            return Some((start, end));
        }
        None
    })
}

fn url_prefix(value: &str) -> Option<&'static str> {
    [
        "https://",
        "http://",
        "www.youtube.com/",
        "youtube.com/",
        "music.youtube.com/",
        "m.youtube.com/",
        "youtu.be/",
    ]
    .into_iter()
    .find(|prefix| value.starts_with(prefix))
}

fn is_url_start_boundary(description: &str, index: usize) -> bool {
    description[..index]
        .chars()
        .next_back()
        .is_none_or(|previous| !previous.is_alphanumeric() && !matches!(previous, '_' | '-'))
}

fn is_url_terminator(character: char) -> bool {
    character.is_whitespace()
        || character.is_control()
        || matches!(character, '<' | '>' | '"' | '\'' | '`')
}

fn trim_url_end(description: &str, start: usize, mut end: usize) -> usize {
    while end > start {
        let Some(last) = description[start..end].chars().next_back() else {
            break;
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}') {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn parse_timecodes(description: &str, links: &mut Vec<DescriptionLink>) {
    let mut index = 0;
    while index < description.len() {
        let Some(character) = description[index..].chars().next() else {
            break;
        };
        if !character.is_ascii_digit()
            || !description[..index]
                .chars()
                .next_back()
                .is_none_or(|previous| {
                    !previous.is_ascii_alphanumeric() && previous != ':' && previous != '_'
                })
        {
            index += character.len_utf8();
            continue;
        }

        let mut end = index;
        for candidate in description[index..].chars() {
            if candidate.is_ascii_digit() || candidate == ':' {
                end += candidate.len_utf8();
            } else {
                break;
            }
        }
        let followed_by_word = description[end..]
            .chars()
            .next()
            .is_some_and(|next| next.is_ascii_alphanumeric() || matches!(next, ':' | '_'));
        if !followed_by_word
            && !overlaps_existing(index, end, links)
            && let Some(seconds) = parse_colon_timecode(&description[index..end])
        {
            links.push(DescriptionLink {
                start_byte: index,
                end_byte: end,
                target: LinkTarget::Timecode { seconds },
            });
        }
        index = end.max(index + character.len_utf8());
    }
}

fn parse_hashtags(description: &str, links: &mut Vec<DescriptionLink>) {
    for (start, character) in description.char_indices() {
        if character != '#'
            || !description[..start]
                .chars()
                .next_back()
                .is_none_or(|previous| {
                    !previous.is_alphanumeric() && !matches!(previous, '_' | '#')
                })
        {
            continue;
        }
        let content_start = start + 1;
        let mut end = content_start;
        for candidate in description[content_start..].chars() {
            if candidate.is_alphanumeric() || candidate == '_' {
                end += candidate.len_utf8();
            } else {
                break;
            }
        }
        if end == content_start || overlaps_existing(start, end, links) {
            continue;
        }
        if let Some(LinkTarget::Hashtag { tag }) = hashtag_target(&description[content_start..end])
        {
            links.push(DescriptionLink {
                start_byte: start,
                end_byte: end,
                target: LinkTarget::Hashtag { tag },
            });
        }
    }
}

fn overlaps_existing(start: usize, end: usize, links: &[DescriptionLink]) -> bool {
    links
        .iter()
        .any(|link| start < link.end_byte && end > link.start_byte)
}

fn parse_colon_timecode(value: &str) -> Option<u64> {
    let parts = value
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    match parts.as_slice() {
        [minutes, seconds] if *seconds < 60 => minutes.checked_mul(60)?.checked_add(*seconds),
        [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => hours
            .checked_mul(3600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds),
        _ => None,
    }
}

fn parse_start_seconds(url: &Url) -> Option<u64> {
    query_value(url, "t")
        .or_else(|| query_value(url, "start"))
        .as_deref()
        .and_then(parse_time_parameter)
}

fn parse_time_parameter(value: &str) -> Option<u64> {
    if value.contains(':') {
        return parse_colon_timecode(value);
    }
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }

    let mut total = 0_u64;
    let mut number = 0_u64;
    let mut has_digits = false;
    let mut used_unit = false;
    for character in value.chars() {
        if let Some(digit) = character.to_digit(10) {
            number = number.checked_mul(10)?.checked_add(u64::from(digit))?;
            has_digits = true;
            continue;
        }
        if !has_digits {
            return None;
        }
        let multiplier = match character {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(multiplier)?)?;
        number = 0;
        has_digits = false;
        used_unit = true;
    }
    if has_digits {
        total = total.checked_add(number)?;
    }
    used_unit.then_some(total)
}

fn query_value<'a>(url: &'a Url, name: &str) -> Option<Cow<'a, str>> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

fn video_target(video_id: &str, start_seconds: Option<u64>) -> Option<LinkTarget> {
    validate_youtube_video_id(video_id).ok()?;
    Some(LinkTarget::YouTubeVideo {
        video_id: video_id.to_owned(),
        start_seconds,
    })
}

fn playlist_target(playlist_id: &str) -> Option<LinkTarget> {
    if playlist_id.is_empty()
        || playlist_id.len() > 128
        || !playlist_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(LinkTarget::YouTubePlaylist {
        playlist_id: playlist_id.to_owned(),
    })
}

fn channel_target(reference_kind: ChannelReferenceKind, reference: &str) -> Option<LinkTarget> {
    if reference.is_empty()
        || reference.len() > 128
        || !reference.chars().all(|character| {
            character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '%')
        })
    {
        return None;
    }
    Some(LinkTarget::YouTubeChannel {
        reference_kind,
        reference: reference.to_owned(),
    })
}

fn hashtag_target(tag: &str) -> Option<LinkTarget> {
    if tag.is_empty()
        || tag.chars().count() > 100
        || !tag
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
    {
        return None;
    }
    Some(LinkTarget::Hashtag {
        tag: tag.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies display replacements without mutating the original source fixture.
    #[cfg(feature = "controller")]
    fn readable_urls(source: &str) -> String {
        let mut output = String::new();
        let mut cursor = 0;
        for escape in description_url_escapes(source) {
            assert!(cursor <= escape.start_byte && escape.start_byte < escape.end_byte);
            output.push_str(&source[cursor..escape.start_byte]);
            output.push_str(&escape.text);
            cursor = escape.end_byte;
        }
        output.push_str(&source[cursor..]);
        output
    }

    /// Unicode labels do not shift existing timecode/video action coordinates.
    #[cfg(feature = "controller")]
    #[test]
    fn url_display_decodes_cyrillic_without_changing_source_or_actions() {
        let source = "🎵 Живая https://commons.wikimedia.org/wiki/File:%D0%91%D0%B5%D1%82%D0%BE%D0%BD.jpg 1:23 https://youtu.be/dQw4w9WgXcQ?t=2m";
        let original = source.to_owned();
        let actions = parse_description_links(source);
        assert_eq!(
            readable_urls(source),
            "🎵 Живая https://commons.wikimedia.org/wiki/File:Бетон.jpg 1:23 https://youtu.be/dQw4w9WgXcQ?t=2m"
        );
        assert_eq!(source, original);
        assert_eq!(parse_description_links(source), actions);
        for escape in description_url_escapes(source) {
            assert!(source.is_char_boundary(escape.start_byte));
            assert!(source.is_char_boundary(escape.end_byte));
            assert!(source[escape.start_byte..escape.end_byte].starts_with('%'));
        }
    }

    /// Encoded combining marks and literal bases remain one mapped grapheme.
    #[cfg(feature = "controller")]
    #[test]
    fn url_display_replacements_include_adjacent_grapheme_bases() {
        let source = "https://example.test/e%CC%81/%D0%95%CC%88/%F0%9F%8E%B5";
        let escapes = description_url_escapes(source);
        assert_eq!(escapes.len(), 3);
        assert_eq!(
            &source[escapes[0].start_byte..escapes[0].end_byte],
            "e%CC%81"
        );
        assert_eq!(escapes[0].text, "e\u{301}");
        assert_eq!(escapes[1].text, "Е\u{308}");
        assert_eq!(escapes[2].text, "🎵");
        assert_eq!(
            readable_urls("https://example.test/%2F%CC%81"),
            "https://example.test/%2F%CC%81"
        );
    }

    /// Delimiters, unsafe text, malformed UTF-8, and repeated escaping stay literal.
    #[cfg(feature = "controller")]
    #[test]
    fn url_display_preserves_reserved_controls_bidi_and_invalid_encodings() {
        for escaped in [
            "%2F",
            "%3F",
            "%23",
            "%25",
            "%20",
            "%2B",
            "%3A",
            "%40",
            "%00",
            "%09",
            "%0A",
            "%0D",
            "%1B",
            "%7F",
            "%C2%85",
            "%E2%80%AE",
            "%E2%80%8E",
            "%E2%81%A6",
            "%EF%BB%BF",
            "%E2%80%8B",
            "%C2%AD",
            "%E2%80%A8",
            "%E2%80%A9",
            "%FF",
            "%D0%9F%FF",
            "%D0%9F%G0",
            "%C0%AF",
            "%ED%A0%80",
            "%G0",
            "%",
            "%D0",
            "%25D0%2591",
        ] {
            let source = format!("https://example.test/{escaped}");
            assert_eq!(
                readable_urls(&source),
                source,
                "unsafe or ambiguous: {escaped}"
            );
        }
    }

    /// Unicode format controls are not covered by Rust's C0/C1 control predicate.
    #[cfg(feature = "controller")]
    #[test]
    fn url_display_keeps_supplementary_and_script_format_controls_encoded() {
        for character in [
            '\u{e0001}',
            '\u{e0061}',
            '\u{1d173}',
            '\u{1bca0}',
            '\u{13430}',
            '\u{110bd}',
            '\u{110cd}',
            '\u{600}',
            '\u{6dd}',
            '\u{70f}',
            '\u{890}',
            '\u{8e2}',
        ] {
            let encoded: String = character
                .to_string()
                .bytes()
                .map(|byte| format!("%{byte:02X}"))
                .collect();
            let source = format!("https://example.test/{encoded}");
            assert_eq!(
                readable_urls(&source),
                source,
                "format U+{:04X}",
                u32::from(character)
            );
        }
    }

    /// Authority spelling and non-URL provider text are never rewritten.
    #[cfg(feature = "controller")]
    #[test]
    fn url_display_preserves_authority_and_ignores_non_urls() {
        let source = "https://%65xample.test/%D0%91?q=%D0%91%2F#%D0%91";
        assert_eq!(readable_urls(source), "https://%65xample.test/Б?q=Б%2F#Б");
        for source in [
            "plain %D0%91 text",
            "ftp://example.test/%D0%91",
            "xhttps://example.test/%D0%91",
        ] {
            assert_eq!(readable_urls(source), source);
        }
    }

    /// Rendering bounds fall back to exact source text, never a clipped label.
    #[cfg(feature = "controller")]
    #[test]
    fn url_display_bounds_fall_back_without_truncating_source() {
        let oversized_field = format!("{} https://example.test/%D0%91", "x".repeat(256 * 1024));
        assert!(description_url_escapes(&oversized_field).is_empty());
        let oversized_url = format!("https://example.test/{}%D0%91", "x".repeat(16 * 1024));
        assert!(description_url_escapes(&oversized_url).is_empty());
        let excessive_replacements = "https://example.test/%C3%A9 ".repeat(4_097);
        assert!(description_url_escapes(&excessive_replacements).is_empty());
        assert_eq!(
            description_url_escapes(&"https://example.test/%C3%A9 ".repeat(4_096)).len(),
            4_096
        );
    }

    #[test]
    fn mixed_description_produces_ordered_non_overlapping_targets() {
        let description = "Start 1:23, then https://youtu.be/dQw4w9WgXcQ?t=2m5s. \
			Visit youtube.com/@Example_Channel and #Music.";
        let links = parse_description_links(description);

        assert_eq!(links.len(), 4);
        assert_eq!(links[0].target, LinkTarget::Timecode { seconds: 83 });
        assert_eq!(
            links[1].target,
            LinkTarget::YouTubeVideo {
                video_id: "dQw4w9WgXcQ".to_owned(),
                start_seconds: Some(125),
            }
        );
        assert_eq!(
            links[2].target,
            LinkTarget::YouTubeChannel {
                reference_kind: ChannelReferenceKind::Handle,
                reference: "Example_Channel".to_owned(),
            }
        );
        assert_eq!(
            links[3].target,
            LinkTarget::Hashtag {
                tag: "Music".to_owned()
            }
        );
        for link in links {
            assert_eq!(
                link.selected_text(description),
                description.get(link.start_byte..link.end_byte)
            );
        }
    }

    #[test]
    fn playlist_channel_and_video_forms_are_classified() {
        let cases = [
            (
                "https://www.youtube.com/playlist?list=PL123_test",
                LinkTarget::YouTubePlaylist {
                    playlist_id: "PL123_test".to_owned(),
                },
            ),
            (
                "https://youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw",
                LinkTarget::YouTubeChannel {
                    reference_kind: ChannelReferenceKind::Id,
                    reference: "UC_x5XG1OV2P6uZZ5FSM9Ttw".to_owned(),
                },
            ),
            (
                "https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=PLignored",
                LinkTarget::YouTubeVideo {
                    video_id: "dQw4w9WgXcQ".to_owned(),
                    start_seconds: None,
                },
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                parse_youtube_url(&Url::parse(raw).expect("test URL should parse")),
                Some(expected)
            );
        }
    }

    #[test]
    fn internal_video_links_include_short_and_watch_urls_only() {
        let description = "\
short https://youtu.be/dQw4w9WgXcQ?t=45
watch https://www.youtube.com/watch?v=9bZkp7q19f0
channel https://www.youtube.com/@Example
playlist https://www.youtube.com/playlist?list=PL123_test";

        let links = parse_description_video_links(description);

        assert_eq!(
            links,
            [
                DescriptionVideoLink {
                    start_byte: description
                        .find("https://youtu.be/dQw4w9WgXcQ?t=45")
                        .expect("short URL should exist"),
                    end_byte: description
                        .find("https://youtu.be/dQw4w9WgXcQ?t=45")
                        .expect("short URL should exist")
                        + "https://youtu.be/dQw4w9WgXcQ?t=45".len(),
                    video_id: "dQw4w9WgXcQ".to_owned(),
                    start_seconds: Some(45),
                },
                DescriptionVideoLink {
                    start_byte: description
                        .find("https://www.youtube.com/watch?v=9bZkp7q19f0")
                        .expect("watch URL should exist"),
                    end_byte: description
                        .find("https://www.youtube.com/watch?v=9bZkp7q19f0")
                        .expect("watch URL should exist")
                        + "https://www.youtube.com/watch?v=9bZkp7q19f0".len(),
                    video_id: "9bZkp7q19f0".to_owned(),
                    start_seconds: None,
                },
            ]
        );
    }

    #[test]
    fn lookalike_hosts_credentials_and_invalid_ids_are_rejected() {
        for raw in [
            "https://notyoutube.com/watch?v=dQw4w9WgXcQ",
            "https://user:secret@youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?v=too-short",
            "ftp://youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(
                parse_youtube_url(&Url::parse(raw).expect("test URL should parse")),
                None
            );
        }
    }

    #[test]
    fn url_timecode_is_not_also_reported_as_standalone_timecode() {
        let description = "https://youtube.com/watch?v=dQw4w9WgXcQ&t=1:30";
        let links = parse_description_links(description);

        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].target,
            LinkTarget::YouTubeVideo {
                video_id: "dQw4w9WgXcQ".to_owned(),
                start_seconds: Some(90),
            }
        );
    }

    #[test]
    fn percent_encoded_query_values_are_decoded_before_validation() {
        let url = Url::parse("https://youtube.com/watch?v=%64Qw4w9WgXcQ&t=1%6D30s")
            .expect("test URL should parse");

        assert_eq!(
            parse_youtube_url(&url),
            Some(LinkTarget::YouTubeVideo {
                video_id: "dQw4w9WgXcQ".to_owned(),
                start_seconds: Some(90),
            })
        );
    }

    #[test]
    fn standalone_timecodes_validate_components_and_word_boundaries() {
        let description = "Valid 01:02:03 and 123:45; invalid 1:99 x2:30y.";
        let links = parse_description_links(description);
        let seconds = links
            .iter()
            .filter_map(|link| match link.target {
                LinkTarget::Timecode { seconds } => Some(seconds),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(seconds, [3723, 7425]);
    }

    #[test]
    fn unicode_hashtag_uses_correct_utf8_byte_range() {
        let description = "Музыка: #аудиокниги 🎧";
        let links = parse_description_links(description);
        let [link] = links.as_slice() else {
            panic!("expected one hashtag");
        };

        assert_eq!(link.selected_text(description), Some("#аудиокниги"));
        assert_eq!(
            link.target,
            LinkTarget::Hashtag {
                tag: "аудиокниги".to_owned()
            }
        );
    }

    #[test]
    fn markdown_wrapped_url_has_clean_selectable_range() {
        let description = "[video](https://youtu.be/dQw4w9WgXcQ), next";
        let links = parse_description_links(description);
        let [link] = links.as_slice() else {
            panic!("expected one video");
        };

        assert_eq!(
            link.selected_text(description),
            Some("https://youtu.be/dQw4w9WgXcQ")
        );
    }

    #[test]
    fn canonical_urls_preserve_labels_and_seek_time() {
        let target = LinkTarget::YouTubeVideo {
            video_id: "dQw4w9WgXcQ".to_owned(),
            start_seconds: Some(90),
        };
        let canonical = target.canonical_url().expect("video has a URL");

        assert_eq!(parse_youtube_url(&canonical), Some(target));
        assert!(
            LinkTarget::Timecode { seconds: 90 }
                .canonical_url()
                .is_none()
        );
    }

    #[test]
    fn unicode_description_timecodes_become_bounded_language_agnostic_chapters() {
        let description = "\
00:00:00 Батуми: грузинские трущобы и модные новостройки
00:01:35 Многоэтажное великолепие Батуми
00:04:58 Грузинский Дубай
00:08:39 Чудеса света в Батуми
00:10:23 Туры в Грузию
00:12:46 Похорошел ли Батуми при Саакашвили?
00:17:11 Здание-бутылка и Белая магнолия
00:21:33 Пустующий Батуми Тауэр
00:25:50 Топ зданий Батуми
00:26:53 Групповые, корпоративные и индивидуальные туры в Грузию
00:29:41 Макдональдс — лучшее здание Батуми?
00:32:26 Традиционные ценности и свобода
00:34:55 Взрослые развлечения в Батуми
00:38:18 Трущобы «Город мечты»
00:43:06 Современная архитектура и историческое наследие
00:47:28 Мои путеводители
00:49:07 Культурная история Батуми
00:54:55 Политический кризс в Грузии
00:58:52 Железная дорога Баку — Батуми
01:02:51 Релокация в Грузию
01:07:23 Фонтан чачи в Батуми
01:12:17 Ночная жизнь в Батуми
01:19:19 Собеседование на закрытую вечеринку
01:23:27 Прогулка по Батуми
01:28:30 Заброшки в Батуми
01:30:01 История афрогрузин
01:32:22 Спасение бездомных животных
01:34:40 Лазы в Грузии
01:39:19 Современная архитектура на границе с Турцией
01:41:40 Заключение";

        let chapters = parse_description_chapters(description, Some(6_200));

        assert_eq!(chapters.len(), 30);
        assert_eq!(
            chapters.first(),
            Some(&DescriptionChapter {
                title: "Батуми: грузинские трущобы и модные новостройки".to_owned(),
                start_seconds: 0,
                end_seconds: Some(95),
                timestamp_start_byte: 0,
                timestamp_end_byte: 8,
            })
        );
        assert_eq!(chapters[18].start_seconds, 3_532);
        assert_eq!(chapters[18].title, "Железная дорога Баку — Батуми");
        assert_eq!(chapters[18].timestamp_text(description), Some("00:58:52"));
        assert_eq!(
            chapters.last().map(|chapter| (
                chapter.title.as_str(),
                chapter.start_seconds,
                chapter.end_seconds,
                chapter.timestamp_text(description),
            )),
            Some(("Заключение", 6_100, Some(6_200), Some("01:41:40")))
        );
        assert!(
            chapters
                .windows(2)
                .all(|pair| pair[0].end_seconds == Some(pair[1].start_seconds))
        );
    }

    #[test]
    fn crlf_and_optional_separators_are_supported() {
        let description =
            "\t00:00 Intro\r\n  00:10 - Second\r\n00:20–Третий\r\n00:30 — Четвёртый\r\n";

        let chapters = parse_description_chapters(description, Some(40));

        assert_eq!(
            chapters
                .iter()
                .map(|chapter| (
                    chapter.title.as_str(),
                    chapter.start_seconds,
                    chapter.end_seconds,
                    chapter.timestamp_text(description),
                ))
                .collect::<Vec<_>>(),
            [
                ("Intro", 0, Some(10), Some("00:00")),
                ("Second", 10, Some(20), Some("00:10")),
                ("Третий", 20, Some(30), Some("00:20")),
                ("Четвёртый", 30, Some(40), Some("00:30")),
            ]
        );
    }

    #[test]
    fn common_list_markers_before_timecodes_become_chapters() {
        let description = "\
➤ 00:00 Вступление
  • 05:45 Дело не в фамилиях, а в системе
	- 07:25 Двойное подчинение
Prose → 09:20 remains an inline timecode";

        let chapters = parse_description_chapters(description, Some(600));

        assert_eq!(
            chapters
                .iter()
                .map(|chapter| (
                    chapter.title.as_str(),
                    chapter.start_seconds,
                    chapter.timestamp_text(description),
                ))
                .collect::<Vec<_>>(),
            [
                ("Вступление", 0, Some("00:00")),
                ("Дело не в фамилиях, а в системе", 345, Some("05:45")),
                ("Двойное подчинение", 445, Some("07:25")),
            ]
        );
        assert_eq!(chapters[2].end_seconds, Some(600));
    }

    #[test]
    fn displayed_chapter_lines_drop_markers_and_one_final_period() {
        let description = "\
➤ 00:00 Обновленный формат эфира.
Ordinary sentence.
  • 05:45 Wait...
Inline ➤ 07:25 remains unchanged\r
";

        assert_eq!(
            normalize_description_chapter_lines(description),
            "\
00:00 Обновленный формат эфира
Ordinary sentence.
05:45 Wait...
Inline ➤ 07:25 remains unchanged\r
"
        );
        assert_eq!(chapter_title_for_display("Title.  "), "Title");
        assert_eq!(chapter_title_for_display("Wait..."), "Wait...");
    }

    #[test]
    fn advertisement_chapter_title_matching_is_normalized_but_intentionally_exact() {
        for title in ["Реклама", " Реклама ", "Реклама.", "Реклама.  "]
        {
            assert!(
                is_advertisement_chapter_title(title),
                "`{title}` should be the exact normalized advertisement label"
            );
        }
        for title in [
            "реклама",
            "РЕКЛАМА",
            "Реклама!",
            "Реклама...",
            "Реклама 2",
            "Рекламная интеграция",
            "Advertisement",
        ] {
            assert!(
                !is_advertisement_chapter_title(title),
                "`{title}` must not be classified as the exact advertisement label"
            );
        }

        let chapters = parse_description_chapters("➤ 00:00 — Реклама.\n00:10 Programme", Some(20));
        assert!(is_advertisement_chapter_title(&chapters[0].title));
        assert!(!is_advertisement_chapter_title(&chapters[1].title));
    }

    #[test]
    fn malformed_inline_empty_and_out_of_duration_markers_are_excluded() {
        let description = "\
Intro at 00:00 is inline
00:99 Malformed
00:05
00:10 Valid
00:20 At duration
00:21 Beyond duration";

        let chapters = parse_description_chapters(description, Some(20));

        assert_eq!(
            chapters,
            [DescriptionChapter {
                title: "Valid".to_owned(),
                start_seconds: 10,
                end_seconds: Some(20),
                timestamp_start_byte: description.find("00:10").expect("fixture contains marker"),
                timestamp_end_byte: description.find("00:10").expect("fixture contains marker") + 5,
            }]
        );
    }

    #[test]
    fn duplicate_and_out_of_order_markers_do_not_break_later_chapters() {
        let description = "\
00:00 First
00:10 Second
00:10 Duplicate
00:05 Out of order
00:20 Third";

        let chapters = parse_description_chapters(description, None);

        assert_eq!(
            chapters
                .iter()
                .map(|chapter| (
                    chapter.title.as_str(),
                    chapter.start_seconds,
                    chapter.end_seconds,
                ))
                .collect::<Vec<_>>(),
            [
                ("First", 0, Some(10)),
                ("Second", 10, Some(20)),
                ("Third", 20, None),
            ]
        );
    }
}
