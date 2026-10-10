//! Bounded declarations from HTML, Open Graph and one unambiguous Schema.org page object.

use std::collections::{BTreeMap, BTreeSet};

use html5gum::{DefaultEmitter, StartTag, Token, Tokenizer};
use serde_json::Value;
use url::Url;

use super::{
    Facts, Failure, MAX_FIELD_BYTES, MAX_HTML_BYTES, MAX_HTML_TOKENS, parse_json, safe_text,
    validate_public_url,
};

const MAX_SCHEMA_DOCUMENTS: usize = 8;
const MAX_SCHEMA_NODES: usize = 128;
const MAX_FEEDS: usize = 8;

/// Head declarations are independent of body JSON-LD and retain their normal priority.
#[derive(Default)]
struct Declarations {
    fields: BTreeMap<&'static str, String>,
    title: String,
    feeds: Vec<Feed>,
    schemas: Vec<Value>,
    base: Option<Url>,
}

/// A feed is only a displayed link, never another network request.
struct Feed {
    label: &'static str,
    title: String,
    href: String,
}

/// Keeps inert template contents and ordinary body markup out of page claims.
#[derive(Default)]
struct ScanState {
    body: bool,
    title: bool,
    hidden: usize,
    script: Option<Vec<u8>>,
}

/// Extracts page claims without running scripts or requesting feeds, contexts or linked objects.
pub(super) fn facts(html: &[u8], base_url: Option<&Url>) -> Result<Vec<String>, Failure> {
    let mut declarations = scan(html, base_url)?;
    declarations.project_schema(base_url);
    Ok(declarations.finish(base_url))
}

/// The existing HTML tokenizer decodes entities; scripts remain raw JSON, never executable code.
fn scan(html: &[u8], base_url: Option<&Url>) -> Result<Declarations, Failure> {
    if html.len() > MAX_HTML_BYTES {
        return Err(Failure::TooLarge);
    }
    let mut declarations = Declarations::default();
    let mut state = ScanState::default();
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    for (index, token) in Tokenizer::new_with_emitter(html, emitter).enumerate() {
        if index >= MAX_HTML_TOKENS {
            // A long body must not discard the head we already read. Incomplete
            // JSON-LD is never committed; an unfinished title is not a full claim.
            if state.title {
                declarations.title.clear();
            }
            break;
        }
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) => {
                let name = tag.name.as_slice();
                if matches!(name, b"template" | b"noscript") {
                    state.hidden = state.hidden.saturating_add(1);
                } else if state.hidden == 0 {
                    if name == b"body" {
                        state.body = true;
                        state.title = false;
                    } else if name == b"script" {
                        if attribute(&tag, b"type")
                            .is_some_and(|value| mime_is(value, "application/ld+json"))
                            && declarations.schemas.len() < MAX_SCHEMA_DOCUMENTS
                        {
                            state.script = Some(Vec::new());
                        }
                    } else if !state.body {
                        if name == b"title" {
                            state.title = declarations.title.is_empty();
                        }
                        declarations.head_tag(&tag, base_url);
                    }
                }
            }
            Token::EndTag(tag) => match tag.name.as_slice() {
                b"template" | b"noscript" => state.hidden = state.hidden.saturating_sub(1),
                b"title" => state.title = false,
                b"head" if state.hidden == 0 => {
                    state.body = true;
                    state.title = false;
                }
                b"script" if state.hidden == 0 => {
                    if let Some(bytes) = state.script.take()
                        && let Ok(value) = parse_json(&bytes)
                    {
                        declarations.schemas.push(value);
                    }
                }
                _ => {}
            },
            Token::String(value) if state.hidden == 0 => {
                if let Some(script) = state.script.as_mut() {
                    script.extend_from_slice(value.value.as_ref());
                } else if state.title && declarations.title.len() < MAX_FIELD_BYTES {
                    let text = String::from_utf8_lossy(value.value.as_ref());
                    let mut end = text.len().min(MAX_FIELD_BYTES - declarations.title.len());
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    declarations.title.push_str(&text[..end]);
                }
            }
            _ => {}
        }
    }
    Ok(declarations)
}

/// Attribute values have already passed HTML entity decoding and remain byte-bounded.
fn attribute<'a>(tag: &'a StartTag<()>, name: &[u8]) -> Option<&'a str> {
    tag.attributes
        .get(name)
        .and_then(|value| std::str::from_utf8(value.value.as_ref()).ok())
        .filter(|value| value.len() <= 8_192)
}

/// MIME parameters do not change the declared media type.
fn mime_is(value: &str, expected: &str) -> bool {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case(expected)
}

impl Declarations {
    /// Records the first nonempty value, preserving explicit head metadata over JSON-LD.
    fn field(&mut self, key: &'static str, value: &str) {
        let value = safe_text(value);
        if !value.is_empty() {
            self.fields.entry(key).or_insert(value);
        }
    }

    /// Collects only declarations relevant to this page, not arbitrary linked resources.
    fn head_tag(&mut self, tag: &StartTag<()>, base_url: Option<&Url>) {
        match tag.name.as_slice() {
            b"html" => {
                if let Some(value) = attribute(tag, b"lang") {
                    self.field("language", value);
                }
            }
            b"base" if self.base.is_none() => {
                self.base = attribute(tag, b"href").and_then(|href| public_link(href, base_url));
            }
            b"link" if self.feeds.len() < MAX_FEEDS * 4 => self.feed(tag),
            b"meta" => {
                let key = attribute(tag, b"name")
                    .or_else(|| attribute(tag, b"property"))
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                if let Some(value) = attribute(tag, b"content") {
                    self.meta(&key, value, base_url);
                }
            }
            _ => {}
        }
    }

    /// Maps the small documented HTML/Open Graph vocabulary into display fields.
    fn meta(&mut self, key: &str, value: &str, base_url: Option<&Url>) {
        let field = match key {
            "description" => "description",
            "og:description" => "fallback-description",
            "og:title" => "fallback-title",
            "author" | "article:author" => "author",
            "publisher" | "article:publisher" => "publisher",
            "og:site_name" => "site",
            "language" | "og:locale" => "language",
            "generator" => "software",
            "og:type" => "type",
            "article:published_time" | "datepublished" => "published",
            "article:modified_time" | "og:updated_time" | "datemodified" => "updated",
            "music:musician" => "artist",
            "music:album" => "album",
            "music:album:track" => "track",
            "music:duration" | "video:duration" => {
                if let Ok(seconds) = value.trim().parse::<u64>() {
                    self.field("duration", &format!("{seconds} seconds"));
                }
                return;
            }
            _ => return,
        };
        if matches!(field, "author" | "publisher" | "artist" | "album") {
            if let Some(value) = name_or_link(value, base_url) {
                self.field(field, &value);
            }
        } else {
            self.field(field, value);
        }
    }

    /// Defers URL resolution until the head's base element is known.
    fn feed(&mut self, tag: &StartTag<()>) {
        if !attribute(tag, b"rel").is_some_and(|rel| {
            rel.split_ascii_whitespace()
                .any(|part| part.eq_ignore_ascii_case("alternate"))
        }) {
            return;
        }
        let Some(kind) = attribute(tag, b"type") else {
            return;
        };
        let label = if mime_is(kind, "application/rss+xml") {
            "RSS feed"
        } else if mime_is(kind, "application/atom+xml") {
            "Atom feed"
        } else {
            return;
        };
        if let Some(href) = attribute(tag, b"href") {
            self.feeds.push(Feed {
                label,
                title: safe_text(attribute(tag, b"title").unwrap_or_default()),
                href: href.to_owned(),
            });
        }
    }

    /// Projects exactly one identified page/work object and its direct local references.
    fn project_schema(&mut self, base_url: Option<&Url>) {
        let nodes = schema_nodes(&self.schemas);
        let Some(node) = select_schema(&nodes, base_url) else {
            return;
        };
        let mut values = Vec::new();
        for (key, property) in [
            ("schema-title", "headline"),
            ("schema-title", "name"),
            ("schema-description", "description"),
            ("published", "datePublished"),
            ("updated", "dateModified"),
            ("duration", "duration"),
            ("track", "trackNumber"),
        ] {
            if let Some(value) = node.get(property).and_then(scalar) {
                values.push((key, value));
            }
        }
        if let Some(kind) = primary_kind(node) {
            values.push(("type", kind.to_owned()));
        }
        for (key, property) in [("author", "author"), ("publisher", "publisher")] {
            if let Some(value) = node
                .get(property)
                .and_then(|value| entity_name(value, &nodes, base_url, 0))
            {
                values.push((key, value));
            }
        }
        if primary_kind(node).is_some_and(is_media) {
            for (key, property) in [
                ("artist", "byArtist"),
                ("artist", "creator"),
                ("album", "inAlbum"),
            ] {
                if let Some(value) = node
                    .get(property)
                    .and_then(|value| entity_name(value, &nodes, base_url, 0))
                {
                    values.push((key, value));
                }
            }
        }
        for (key, value) in values {
            self.field(key, &value);
        }
    }

    /// Labels declarations as website claims and never fabricates missing values.
    fn finish(self, base_url: Option<&Url>) -> Vec<String> {
        let title = safe_text(&self.title);
        let title = if title.is_empty() {
            self.fields
                .get("fallback-title")
                .or_else(|| self.fields.get("schema-title"))
                .map_or("", String::as_str)
        } else {
            &title
        };
        let description = self
            .fields
            .get("description")
            .or_else(|| self.fields.get("fallback-description"))
            .or_else(|| self.fields.get("schema-description"))
            .map_or("", String::as_str);
        let mut facts = Facts::default();
        facts.add("Title", &title.replace('—', "-"));
        facts.add("Description", &description.replace('—', "-"));
        for (key, label) in [
            ("author", "Author (website claim)"),
            ("site", "Site"),
            ("language", "Language"),
            ("software", "Software (website claim)"),
            ("publisher", "Publisher (website claim)"),
            ("type", "Page type (website claim)"),
            ("published", "Published (website claim)"),
            ("updated", "Updated (website claim)"),
            ("artist", "Artist (website claim)"),
            ("album", "Album (website claim)"),
            ("duration", "Duration (website claim)"),
            ("track", "Track number (website claim)"),
        ] {
            if let Some(value) = self.fields.get(key) {
                facts.add(label, value);
            }
        }
        let mut urls = BTreeSet::new();
        for feed in self.feeds {
            let Some(mut url) = public_link(&feed.href, self.base.as_ref().or(base_url)) else {
                continue;
            };
            url.set_fragment(None);
            // Do not turn truncation into a different clickable destination.
            if url.as_str().len() > MAX_FIELD_BYTES / 2 || !urls.insert(url.to_string()) {
                continue;
            }
            let mut title = feed.title;
            let mut end = title.len().min(128);
            while !title.is_char_boundary(end) {
                end -= 1;
            }
            title.truncate(end);
            let value = if title.is_empty() {
                url.to_string()
            } else {
                format!("{title} - {url}")
            };
            facts.add(feed.label, &value);
            if urls.len() == MAX_FEEDS {
                break;
            }
        }
        facts.finish()
    }
}

/// Resolves link declarations syntactically; metadata extraction never performs DNS.
fn public_link(value: &str, base_url: Option<&Url>) -> Option<Url> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(value)
        .ok()
        .or_else(|| base_url.and_then(|base| base.join(value).ok()))?;
    validate_public_url(&url).ok()?;
    Some(url)
}

/// Human names are plain text; URL-shaped claims must satisfy the public-link policy.
fn name_or_link(value: &str, base_url: Option<&Url>) -> Option<String> {
    let value = value.trim();
    if Url::parse(value).is_ok() || value.starts_with(['/', '#']) {
        return public_link(value, base_url).map(|url| url.to_string());
    }
    let value = safe_text(value);
    (!value.is_empty()).then_some(value)
}

/// Only root and graph objects are candidates; recommendation/list descendants stay opaque.
fn schema_nodes(documents: &[Value]) -> Vec<&Value> {
    let mut nodes = Vec::new();
    for document in documents {
        let roots = document
            .as_array()
            .map_or_else(|| std::slice::from_ref(document), Vec::as_slice);
        for root in roots {
            if root.is_object() && nodes.len() < MAX_SCHEMA_NODES {
                nodes.push(root);
                if let Some(graph) = root.get("@graph").and_then(Value::as_array) {
                    nodes.extend(
                        graph
                            .iter()
                            .filter(|node| node.is_object())
                            .take(MAX_SCHEMA_NODES - nodes.len()),
                    );
                }
            }
        }
    }
    nodes
}

/// Recognizes content, not site navigation, people or organization reference objects.
fn primary_kind(node: &Value) -> Option<&str> {
    let kinds = node.get("@type")?;
    let kinds = kinds
        .as_array()
        .map_or_else(|| std::slice::from_ref(kinds), Vec::as_slice);
    kinds
        .iter()
        .filter_map(Value::as_str)
        .map(|kind| {
            kind.strip_prefix("https://schema.org/")
                .or_else(|| kind.strip_prefix("http://schema.org/"))
                .unwrap_or(kind)
        })
        .find(|kind| {
            matches!(
                *kind,
                "WebPage" | "Article" | "NewsArticle" | "BlogPosting" | "TechArticle"
            ) || is_media(kind)
        })
}

/// Media-only properties are not borrowed from arbitrary article creators or list children.
fn is_media(kind: &str) -> bool {
    matches!(
        kind,
        "MusicRecording"
            | "MusicAlbum"
            | "PodcastEpisode"
            | "AudioObject"
            | "VideoObject"
            | "Movie"
    )
}

/// Returns a single match; ambiguous JSON-LD never overrides trustworthy head metadata.
fn unique<'a>(mut candidates: impl Iterator<Item = &'a Value>) -> Option<&'a Value> {
    let candidate = candidates.next()?;
    candidates.next().is_none().then_some(candidate)
}

/// A mainEntity relationship has priority over other same-page fragment identifiers.
fn select_schema<'a>(nodes: &[&'a Value], base_url: Option<&Url>) -> Option<&'a Value> {
    let candidates: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|node| primary_kind(node).is_some())
        .collect();
    let main: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|node| {
            primary_kind(node) == Some("WebPage")
                && (matches_page(node, base_url) || (candidates.len() == 1 && !has_identity(node)))
        })
        .filter_map(|node| node.get("mainEntity"))
        .filter_map(|value| resolve_reference(value, nodes, base_url))
        .filter(|node| primary_kind(node).is_some())
        .collect();
    if !main.is_empty() {
        return unique(main.into_iter());
    }
    let matching: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|node| matches_page(node, base_url))
        .collect();
    if !matching.is_empty() {
        let content: Vec<_> = matching
            .iter()
            .copied()
            .filter(|node| primary_kind(node) != Some("WebPage"))
            .collect();
        return if content.is_empty() {
            unique(matching.into_iter())
        } else {
            unique(content.into_iter())
        };
    }
    unique(candidates.into_iter()).filter(|node| !has_identity(node))
}

/// Explicit foreign identities cannot be treated as an unlabelled current-page object.
fn has_identity(node: &Value) -> bool {
    ["url", "@id", "mainEntityOfPage"]
        .iter()
        .any(|key| node.get(key).is_some())
}

/// URL fragments identify page objects, while path/query changes identify another page.
fn matches_page(node: &Value, base_url: Option<&Url>) -> bool {
    let Some(base) = base_url else { return false };
    // A local graph identifier cannot make an explicitly different URL the current page.
    if let Some(value) = node.get("url") {
        let Some(mut url) = identity(value).and_then(|value| public_link(value, Some(base))) else {
            return false;
        };
        let mut base = base.clone();
        url.set_fragment(None);
        base.set_fragment(None);
        if url != base {
            return false;
        }
    }
    ["url", "@id", "mainEntityOfPage"]
        .iter()
        .filter_map(|key| node.get(key))
        .filter_map(identity)
        .filter_map(|value| public_link(value, Some(base)))
        .any(|mut url| {
            let mut base = base.clone();
            url.set_fragment(None);
            base.set_fragment(None);
            url == base
        })
}

/// Extracts only an identity, never metadata from nested recommendation objects.
fn identity(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| {
        value
            .get("@id")
            .or_else(|| value.get("url"))
            .and_then(Value::as_str)
    })
}

/// Resolves references solely against the already-parsed local graph, without fetching them.
fn resolve_reference<'a>(
    value: &'a Value,
    nodes: &[&'a Value],
    base_url: Option<&Url>,
) -> Option<&'a Value> {
    if value
        .as_object()
        .is_some_and(|object| object.keys().any(|key| key != "@id"))
    {
        return Some(value);
    }
    let id = identity(value)?;
    unique(nodes.iter().copied().filter(|node| {
        node.get("@id")
            .and_then(Value::as_str)
            .is_some_and(|candidate| {
                candidate == id
                    || public_link(id, base_url)
                        .is_some_and(|id| public_link(candidate, base_url).as_ref() == Some(&id))
            })
    }))
}

/// Names take priority over public URLs, with bounded local-reference and list traversal.
fn entity_name(
    value: &Value,
    nodes: &[&Value],
    base_url: Option<&Url>,
    depth: usize,
) -> Option<String> {
    if depth > 4 {
        return None;
    }
    if let Some(values) = value.as_array() {
        let mut names = BTreeSet::new();
        for item in values.iter().take(8) {
            if let Some(name) = entity_name(item, nodes, base_url, depth + 1) {
                names.insert(name);
            }
        }
        return (!names.is_empty()).then(|| names.into_iter().collect::<Vec<_>>().join(", "));
    }
    if let Some(name) = value.as_str() {
        return name_or_link(name, base_url);
    }
    if let Some(name) = value
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| name_or_link(name, base_url))
    {
        return Some(name);
    }
    if let Some(reference) = resolve_reference(value, nodes, base_url)
        && !std::ptr::eq(reference, value)
    {
        return entity_name(reference, nodes, base_url, depth + 1);
    }
    value
        .get("url")
        .or_else(|| value.get("@id"))
        .and_then(Value::as_str)
        .and_then(|url| public_link(url, base_url))
        .map(|url| url.to_string())
}

/// Numeric metadata remains numeric; objects and booleans are never formatted as user text.
fn scalar(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    fn page(html: &str) -> Vec<String> {
        facts(
            html.as_bytes(),
            Some(&Url::parse("https://example.com/music/song").unwrap()),
        )
        .unwrap()
    }

    #[test]
    fn preserves_title_description_priority_ascii_dashes_and_existing_fields() {
        assert_eq!(
            page(
                "<html lang='ru'><head><title>Песня &amp; музыка — название</title><meta property='og:title' content='Other'><meta name='description' content='A &mdash; B'><meta property='og:description' content='Fallback'><meta name='author' content='Музыкант'><meta property='og:site_name' content='Сайт — имя'><meta name='generator' content='WordPress 6.8'></head><body><meta name='author' content='Wrong'></body>"
            ),
            [
                "Title: Песня & музыка - название",
                "Description: A - B",
                "Author (website claim): Музыкант",
                "Site: Сайт — имя",
                "Language: ru",
                "Software (website claim): WordPress 6.8"
            ]
        );
        assert_eq!(facts(b"<meta property='og:title' content='Fallback'><meta property='og:description' content='A &amp; B'>", None).unwrap(), ["Title: Fallback", "Description: A & B"]);
    }

    #[test]
    fn open_graph_publication_and_music_claims_are_bounded_and_named() {
        let values = page(
            "<meta property='og:type' content='music.song'><meta property='article:published_time' content='2020-01-02'><meta property='article:modified_time' content='2021-03-04'><meta name='publisher' content='Publisher'><meta property='music:musician' content='https://example.com/artist'><meta property='music:album' content='https://example.com/album'><meta property='music:duration' content='240'><meta property='music:album:track' content='7'>",
        );
        for expected in [
            "Page type (website claim): music.song",
            "Published (website claim): 2020-01-02",
            "Updated (website claim): 2021-03-04",
            "Publisher (website claim): Publisher",
            "Artist (website claim): https://example.com/artist",
            "Album (website claim): https://example.com/album",
            "Duration (website claim): 240 seconds",
            "Track number (website claim): 7",
        ] {
            assert!(
                values.iter().any(|value| value == expected),
                "{expected}: {values:?}"
            );
        }
    }

    #[test]
    fn feeds_resolve_against_final_url_or_public_base_and_deduplicate() {
        let values = page(
            "<head><link rel='alternate' type='application/rss+xml' title='News &amp; music' href='../feed'><base href='https://feeds.example.com/path/'><link rel='ALTERNATE' type='application/rss+xml' href='../feed'><link rel='alternate' type='application/atom+xml' href='atom.xml'></head><body><link rel='alternate' type='application/rss+xml' href='wrong.xml'></body>",
        );
        assert_eq!(
            values,
            [
                "RSS feed: News & music - https://feeds.example.com/feed",
                "Atom feed: https://feeds.example.com/path/atom.xml"
            ]
        );
        assert_eq!(
            page(
                "<base href='http://127.0.0.1/'><link rel='alternate' type='application/rss+xml' href='../feed'>"
            ),
            ["RSS feed: https://example.com/feed"]
        );
        assert!(
            facts(
                b"<link rel='alternate' type='application/rss+xml' href='/feed'>",
                None
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn feeds_and_person_urls_reject_unsafe_schemes_private_hosts_and_credentials() {
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "http://localhost/",
            "http://127.0.0.1/",
            "https://secret:password@example.com/",
            "https://example.com:8443/",
        ] {
            let values = page(&format!(
                "<link rel='alternate' type='application/rss+xml' href='{url}'><meta name='author' content='{url}'><meta name='publisher' content='{url}'>"
            ));
            assert!(values.is_empty(), "{url}: {values:?}");
        }
        let mut html = String::new();
        for n in 0..20 {
            write!(
                html,
                "<link rel='alternate' type='application/rss+xml' href='/feed{n}'>"
            )
            .unwrap();
        }
        assert_eq!(page(&html).len(), 8);
    }

    #[test]
    fn long_unicode_feed_titles_do_not_truncate_the_clickable_destination() {
        let url = format!("https://example.com/{}", "x".repeat(480));
        let lines = page(&format!(
            "<link rel='alternate' type='application/rss+xml' title='{}' href='{url}'>",
            "音".repeat(150)
        ));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with(&url));
    }

    #[test]
    fn related_url_does_not_match_current_page_only_because_of_local_graph_id() {
        assert!(page(r##"<script type='application/ld+json'>{"@type":"MusicRecording","@id":"#related","url":"https://example.com/other","name":"Wrong"}</script>"##).is_empty());
        let lines = page(
            r#"<script type='application/ld+json'>{"@type":"MusicRecording","name":"Main","byArtist":{"@id":"https://example.com/artist"}}</script>"#,
        );
        assert!(lines.contains(&"Artist (website claim): https://example.com/artist".into()));
    }

    #[test]
    fn schema_music_recording_extracts_only_selected_item_claims() {
        let values = page(
            r#"<script type='application/ld+json'>{"@context":"https://schema.org","@type":"MusicRecording","url":"https://example.com/music/song","name":"Song — title","description":"Music — description","byArtist":{"@type":"MusicGroup","name":"Artist"},"inAlbum":{"@type":"MusicAlbum","name":"Album"},"duration":"PT4M","trackNumber":7,"datePublished":"2020-01-01","dateModified":"2021-01-01","author":{"name":"Writer"},"publisher":{"name":"Label"},"isRelatedTo":{"@type":"MusicRecording","name":"Wrong song","byArtist":{"name":"Wrong artist"}}}</script>"#,
        );
        for expected in [
            "Title: Song - title",
            "Description: Music - description",
            "Page type (website claim): MusicRecording",
            "Artist (website claim): Artist",
            "Album (website claim): Album",
            "Duration (website claim): PT4M",
            "Track number (website claim): 7",
            "Author (website claim): Writer",
            "Publisher (website claim): Label",
        ] {
            assert!(
                values.iter().any(|value| value == expected),
                "{expected}: {values:?}"
            );
        }
        assert!(!values.join("\n").contains("Wrong"));
    }

    #[test]
    fn graph_main_entity_and_local_person_references_are_resolved_without_crawling() {
        let values = page(
            r##"<script type='application/ld+json'>{"@context":"https://schema.org","@graph":[{"@type":"WebPage","@id":"https://example.com/music/song#webpage","mainEntity":{"@id":"#track"}},{"@type":"MusicRecording","@id":"#track","name":"Main track","byArtist":{"@id":"#artist"}},{"@type":"Person","@id":"#artist","name":"Main artist"},{"@type":"MusicRecording","url":"https://example.com/recommended","name":"Wrong track","byArtist":{"name":"Wrong artist"}},{"@type":"BreadcrumbList","itemListElement":[{"@type":"MusicRecording","name":"Wrong breadcrumb"}]}]}</script>"##,
        );
        assert!(values.contains(&"Title: Main track".into()));
        assert!(values.contains(&"Artist (website claim): Main artist".into()));
        assert!(!values.join("\n").contains("Wrong"));
    }

    #[test]
    fn ambiguous_or_unrelated_json_ld_is_not_projected_as_main_page() {
        for json in [
            r#"[{"@type":"MusicRecording","name":"One"},{"@type":"MusicRecording","name":"Two"}]"#,
            r#"{"@type":"MusicRecording","url":"https://other.example/song","name":"Wrong"}"#,
            r#"{"@type":"ItemList","itemListElement":[{"@type":"MusicRecording","name":"Wrong"}]}"#,
            r#"{"@type":"BreadcrumbList","itemListElement":[{"@type":"MusicRecording","name":"Wrong"}]}"#,
        ] {
            let values = page(&format!(
                "<title>Page</title><script type='application/ld+json'>{json}</script>"
            ));
            assert_eq!(values, ["Title: Page"], "{json}");
        }
    }

    #[test]
    fn podcast_video_and_album_root_objects_provide_declared_media_data() {
        for kind in ["PodcastEpisode", "VideoObject", "MusicAlbum"] {
            let values = page(&format!(
                r#"<script type='application/ld+json'>{{"@type":"{kind}","name":"Main work","creator":{{"name":"Creator"}},"duration":"PT15M","datePublished":"2020-01-01"}}</script>"#
            ));
            assert!(
                values.contains(&format!("Page type (website claim): {kind}")),
                "{values:?}"
            );
            assert!(values.contains(&"Artist (website claim): Creator".into()));
            assert!(values.contains(&"Duration (website claim): PT15M".into()));
        }
    }

    #[test]
    fn body_json_ld_is_allowed_but_body_meta_and_executable_scripts_are_not() {
        let values = page(
            r#"<head><title>Main</title></head><body><meta name='generator' content='Wrong'><script>"<meta name='author' content='Wrong'>"</script><template><script type='application/ld+json'>{"@type":"Article","author":"Wrong"}</script></template><script type='application/ld+json'>{"@type":"Article","author":{"name":"Right"}}</script></body>"#,
        );
        assert!(values.contains(&"Author (website claim): Right".into()));
        assert!(!values.join("\n").contains("Wrong"));
    }

    #[test]
    fn malformed_or_deep_schema_does_not_discard_valid_html_metadata() {
        for json in [
            "{broken".to_owned(),
            format!("{}0{}", "[".repeat(40), "]".repeat(40)),
        ] {
            assert_eq!(
                page(&format!(
                    "<title>Useful</title><script type='application/ld+json'>{json}</script>"
                )),
                ["Title: Useful"]
            );
        }
        assert_eq!(
            facts(&vec![b'x'; super::super::MAX_HTML_BYTES + 1], None),
            Err(Failure::TooLarge)
        );
        let html = format!(
            "<title>Bounded</title>{}<meta name='author' content='Too late'>",
            "<br>".repeat(super::super::MAX_HTML_TOKENS)
        );
        assert_eq!(facts(html.as_bytes(), None).unwrap(), ["Title: Bounded"]);
    }

    #[test]
    fn completed_head_claims_survive_a_large_body_without_reading_late_schema() {
        let html = format!(
            "<head><title>Useful page</title><meta name='description' content='Keep this'></head><body>{}<script type='application/ld+json'>{{\"@type\":\"Article\",\"author\":\"Too late\"}}</script>",
            "<br>".repeat(super::super::MAX_HTML_TOKENS)
        );
        assert_eq!(
            page(&html),
            ["Title: Useful page", "Description: Keep this"]
        );
    }

    #[test]
    fn absent_claims_stay_absent_and_values_remain_terminal_safe() {
        assert!(page("<html><head></head><body>Nothing declared</body></html>").is_empty());
        let values = page(
            "<meta name='generator' content='App &#x1b;[31m&#x202e; software'><meta name='author' content='Author'><meta name='author' content='Duplicate'>",
        );
        assert_eq!(
            values
                .iter()
                .filter(|line| line.starts_with("Author"))
                .count(),
            1
        );
        assert!(!values.join("").contains('\u{1b}'));
        assert!(!values.join("").contains('\u{202e}'));
    }
}
