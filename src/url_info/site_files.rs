//! Explicit, bounded robots.txt display and lazy sitemap browsing.
//!
//! Sitemap entries are navigation data, not fetch instructions: only the controller's
//! explicit selection loads a child. XML never resolves external/custom entities.

use super::{
    Budget, DocumentKind, Failure, HttpTransport, UreqTransport, fetch_document,
    validate_public_url,
};
use quick_xml::{Reader, XmlVersion, events::Event};
use std::sync::atomic::AtomicBool;
use url::Url;

/// Maximum complete file size, including decompressed gzip data; never a silent prefix.
pub(super) const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_XML_DEPTH: usize = 24;
const MAX_XML_EVENTS: usize = 131_072;
const MAX_ENTRIES: usize = 8_192;
const MAX_METADATA_FIELDS: usize = 64;
const MAX_FIELD_BYTES: usize = 4_096;
const MAX_NAME_BYTES: usize = 128;

/// A fetched root file or explicitly selected child, with its final safe destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SiteFile {
    pub(crate) url: Url,
    pub(crate) content: SiteFileContent,
}

/// Robots remain readable text; sitemaps become selectable, unverified URL declarations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SiteFileContent {
    Robots(String),
    Sitemap {
        index: bool,
        entries: Vec<SitemapEntry>,
    },
}

/// One declared child sitemap or page URL and the metadata actually supplied for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SitemapEntry {
    pub(crate) url: Url,
    pub(crate) metadata: Vec<(String, String)>,
}

/// Selects a root file only after validating the original origin and its credentials.
pub(crate) fn target(origin: &Url, sitemap: bool) -> Result<Url, String> {
    validate_public_url(origin).map_err(file_error)?;
    let mut target = origin.clone();
    target.set_path(if sitemap {
        "/sitemap.xml"
    } else {
        "/robots.txt"
    });
    target.set_query(None);
    target.set_fragment(None);
    Ok(target)
}

/// Loads one root file only when the controller explicitly requests it.
pub(crate) fn lookup(
    origin: &Url,
    sitemap: bool,
    cancelled: &AtomicBool,
) -> Result<SiteFile, String> {
    lookup_with(
        &target(origin, sitemap)?,
        sitemap,
        cancelled,
        &UreqTransport::default(),
    )
}

/// Loads exactly one explicitly selected child sitemap, without eager recursion.
pub(crate) fn lookup_sitemap(url: &Url, cancelled: &AtomicBool) -> Result<SiteFile, String> {
    lookup_with(url, true, cancelled, &UreqTransport::default())
}

/// Uses the existing public-address, redirect, credential, TLS, and shared-budget policy.
fn lookup_with(
    url: &Url,
    sitemap: bool,
    cancelled: &AtomicBool,
    transport: &impl HttpTransport,
) -> Result<SiteFile, String> {
    validate_public_url(url).map_err(file_error)?;
    let budget = Budget::new(cancelled);
    let document =
        fetch_document(transport, url, DocumentKind::Text, &budget).map_err(file_error)?;
    if document.response.status == 404 {
        return Err(format!(
            "{} was not found on this website",
            if sitemap { "The sitemap" } else { "robots.txt" }
        ));
    }
    if !(200..300).contains(&document.response.status) {
        return Err(format!(
            "The website returned HTTP {}",
            document.response.status
        ));
    }
    let mime = document
        .response
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if !sitemap && !mime.eq_ignore_ascii_case("text/plain") {
        return Err("robots.txt did not return plain text".to_owned());
    }
    let bytes = if sitemap && document.response.body.starts_with(&[0x1f, 0x8b]) {
        let decoder = flate2::read::MultiGzDecoder::new(document.response.body.as_slice());
        super::read_body(decoder, MAX_FILE_BYTES, cancelled).map_err(|failure| {
            if failure == Failure::Transport {
                "The compressed sitemap is malformed".to_owned()
            } else {
                file_error(failure)
            }
        })?
    } else {
        document.response.body
    };
    budget.remaining().map_err(file_error)?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| "The file is not valid UTF-8 text".to_owned())?;
    let content = if sitemap {
        parse_sitemap(text, &document.url, &budget)?
    } else {
        SiteFileContent::Robots(sanitize_text(text))
    };
    Ok(SiteFile {
        url: document.url,
        content,
    })
}

/// Fixed file-specific messages do not expose response bodies or credential-bearing URLs.
fn file_error(failure: Failure) -> String {
    match failure {
        Failure::TooLarge => "The file exceeds the 1 MiB size limit".to_owned(),
        Failure::WrongType => {
            "The website did not return a supported text or sitemap file".to_owned()
        }
        Failure::Timeout => "The file request timed out".to_owned(),
        _ => failure.message(),
    }
}

/// Parses one sitemap without requesting any entry or expanding custom entities.
fn parse_sitemap(text: &str, url: &Url, budget: &Budget<'_>) -> Result<SiteFileContent, String> {
    if text.len() > MAX_FILE_BYTES {
        return Err(file_error(Failure::TooLarge));
    }
    let mut reader = Reader::from_str(text);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut state = SitemapParser::default();
    for _ in 0..MAX_XML_EVENTS {
        budget.remaining().map_err(file_error)?;
        match reader.read_event().map_err(|_| invalid_xml())? {
            Event::Start(element) => {
                for attribute in element.attributes() {
                    attribute
                        .map_err(|_| invalid_xml())?
                        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                        .map_err(|_| invalid_xml())?;
                }
                let name = std::str::from_utf8(element.name().as_ref())
                    .map_err(|_| invalid_xml())?
                    .to_owned();
                state.start(name)?;
            }
            Event::End(_) => state.end(url)?,
            Event::Text(text) => {
                state.text(&text.xml10_content().map_err(|_| invalid_xml())?)?;
            }
            Event::CData(text) => {
                state.text(&text.xml10_content().map_err(|_| invalid_xml())?)?;
            }
            Event::GeneralRef(reference) => {
                if let Some(character) = reference.resolve_char_ref().map_err(|_| invalid_xml())? {
                    state.text(character.encode_utf8(&mut [0; 4]))?;
                } else {
                    let name = reference.decode().map_err(|_| invalid_xml())?;
                    state.text(match name.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "apos" => "'",
                        "quot" => "\"",
                        _ => return Err("Custom XML entities are not supported".to_owned()),
                    })?;
                }
            }
            Event::DocType(_) => return Err("XML document types are not supported".to_owned()),
            Event::Decl(declaration) => {
                if declaration.encoding().is_some_and(|encoding| {
                    encoding.map_or(true, |value| !value.eq_ignore_ascii_case(b"utf-8"))
                }) {
                    return Err("The sitemap must use UTF-8 text".to_owned());
                }
            }
            Event::Eof => {
                if !state.root_seen || !state.stack.is_empty() || state.pending.is_some() {
                    return Err(invalid_xml());
                }
                return Ok(SiteFileContent::Sitemap {
                    index: state.index,
                    entries: state.entries,
                });
            }
            Event::PI(_) | Event::Comment(_) => {}
            Event::Empty(_) => return Err(invalid_xml()),
        }
    }
    Err("The sitemap exceeds the XML event limit".to_owned())
}

/// Iterative parsing avoids unbounded recursion and keeps metadata separate from navigation.
#[derive(Default)]
struct SitemapParser {
    root_seen: bool,
    index: bool,
    stack: Vec<XmlElement>,
    pending: Option<PendingEntry>,
    entries: Vec<SitemapEntry>,
}

/// One open XML element with bounded leaf text and its original qualified metadata name.
struct XmlElement {
    name: String,
    text: String,
    has_child: bool,
}

/// An entry is emitted only after its location and all metadata pass validation.
#[derive(Default)]
struct PendingEntry {
    location: Option<String>,
    metadata: Vec<(String, String)>,
}

impl SitemapParser {
    /// Starts an element only within the expected sitemap-index or URL-set structure.
    fn start(&mut self, name: String) -> Result<(), String> {
        if self.stack.len() >= MAX_XML_DEPTH || name.len() > MAX_NAME_BYTES {
            return Err("The sitemap exceeds the XML nesting or name limit".to_owned());
        }
        match self.stack.len() {
            0 => {
                if self.root_seen {
                    return Err(invalid_xml());
                }
                self.index = match local_name(&name) {
                    "sitemapindex" => true,
                    "urlset" => false,
                    _ => return Err("The file is not a sitemap index or URL set".to_owned()),
                };
                self.root_seen = true;
            }
            1 => {
                if local_name(&name) != if self.index { "sitemap" } else { "url" } {
                    return Err(invalid_xml());
                }
                self.pending = Some(PendingEntry::default());
            }
            _ => {}
        }
        if let Some(parent) = self.stack.last_mut() {
            parent.has_child = true;
        }
        self.stack.push(XmlElement {
            name,
            text: String::new(),
            has_child: false,
        });
        Ok(())
    }

    /// Only leaf fields retain text; formatting whitespace consumes no persistent memory.
    fn text(&mut self, text: &str) -> Result<(), String> {
        if self.stack.len() <= 2 {
            return if text.trim().is_empty() {
                Ok(())
            } else {
                Err(invalid_xml())
            };
        }
        let element = self.stack.last_mut().ok_or_else(invalid_xml)?;
        if element.text.len().saturating_add(text.len()) > MAX_FIELD_BYTES {
            return Err("The sitemap exceeds the field size limit".to_owned());
        }
        element.text.push_str(text);
        Ok(())
    }

    /// Finishes leaf metadata, or validates one declared URL without fetching it.
    fn end(&mut self, base: &Url) -> Result<(), String> {
        let element = self.stack.pop().ok_or_else(invalid_xml)?;
        if self.stack.len() >= 2 {
            let pending = self.pending.as_mut().ok_or_else(invalid_xml)?;
            if self.stack.len() == 2 && local_name(&element.name) == "loc" {
                if element.has_child || pending.location.is_some() {
                    return Err(invalid_xml());
                }
                pending.location = Some(element.text);
            } else if !element.has_child && !element.text.trim().is_empty() {
                if pending.metadata.len() >= MAX_METADATA_FIELDS {
                    return Err("The sitemap exceeds the metadata field limit".to_owned());
                }
                let label = if self.stack.len() == 2 {
                    match local_name(&element.name) {
                        "lastmod" => "Last modified".to_owned(),
                        "changefreq" => "Change frequency".to_owned(),
                        "priority" => "Priority".to_owned(),
                        _ => element.name.clone(),
                    }
                } else {
                    self.stack
                        .iter()
                        .skip(2)
                        .map(|element| element.name.as_str())
                        .chain(std::iter::once(element.name.as_str()))
                        .collect::<Vec<_>>()
                        .join(" / ")
                };
                pending
                    .metadata
                    .push((single_line(&label), single_line(&element.text)));
            }
        } else if self.stack.len() == 1 {
            let pending = self.pending.take().ok_or_else(invalid_xml)?;
            let location = pending.location.ok_or_else(invalid_xml)?;
            let location = location.trim();
            if location.is_empty()
                || location.chars().any(char::is_control)
                || sanitize_text(location) != location
            {
                return Err("The sitemap contains an invalid URL".to_owned());
            }
            let url = base
                .join(location)
                .map_err(|_| "The sitemap contains an invalid URL".to_owned())?;
            validate_public_url(&url).map_err(file_error)?;
            if self.entries.len() >= MAX_ENTRIES {
                return Err("The sitemap exceeds the entry limit".to_owned());
            }
            self.entries.push(SitemapEntry {
                url,
                metadata: pending.metadata,
            });
        }
        Ok(())
    }
}

/// Namespace prefixes are retained in metadata labels but do not change sitemap roles.
fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Metadata remains plain, compact text, never terminal escape sequences or hyperlinks.
fn single_line(text: &str) -> String {
    sanitize_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// XML errors deliberately omit potentially private or terminal-active response content.
fn invalid_xml() -> String {
    "The sitemap XML is malformed or missing a location".to_owned()
}

/// Removes complete ANSI/C1 sequences and bidi controls while keeping readable line breaks.
fn sanitize_text(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        Intermediate,
        Csi,
        String,
        StringEscape,
    }
    let mut state = State::Text;
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        state = match state {
            State::Text => match character {
                '\u{1b}' => State::Escape,
                '\u{9b}' => State::Csi,
                '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => State::String,
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    result.push('\n');
                    State::Text
                }
                '\n' => {
                    result.push('\n');
                    State::Text
                }
                '\t' => {
                    result.push(' ');
                    State::Text
                }
                character
                    if character.is_control()
                        || matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}') =>
                {
                    State::Text
                }
                character => {
                    result.push(character);
                    State::Text
                }
            },
            State::Escape => match character {
                '[' => State::Csi,
                ']' | 'P' | 'X' | '^' | '_' => State::String,
                '\u{20}'..='\u{2f}' => State::Intermediate,
                _ => State::Text,
            },
            State::Intermediate => match character {
                '\u{1b}' => State::Escape,
                '\u{30}'..='\u{7e}' => State::Text,
                _ => State::Intermediate,
            },
            State::Csi => match character {
                '\u{1b}' => State::Escape,
                '\u{9c}' | '\u{40}'..='\u{7e}' => State::Text,
                _ => State::Csi,
            },
            State::String => match character {
                '\u{7}' | '\u{9c}' => State::Text,
                '\u{1b}' => State::StringEscape,
                _ => State::String,
            },
            State::StringEscape => match character {
                '\\' | '\u{7}' | '\u{9c}' => State::Text,
                '\u{1b}' => State::StringEscape,
                _ => State::String,
            },
        };
    }
    result
}

#[cfg(test)]
#[path = "site_files_tests.rs"]
mod tests;
