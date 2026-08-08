//! Credential-free, bounded artist biographies from public `Last.fm` wiki pages.

use std::sync::Arc;
use std::time::{Duration, Instant};

use url::Url;

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError, provider_agent};

const LASTFM_BASE_URL: &str = "https://www.last.fm/";
const MAX_ARTIST_NAME_BYTES: usize = 1_024;
const MAX_ARTIST_NAME_CHARS: usize = 256;
const MAX_REDIRECTS: usize = 3;
const MAX_WIKI_CONTAINERS: usize = 8;
const MAX_HTML_NESTING: usize = 128;

/// Maximum encoded byte length accepted for one public `Last.fm` wiki page.
pub const MAX_LASTFM_WIKI_HTML_BYTES: usize = 2 * 1024 * 1024;

/// Maximum encoded byte length accepted for one extracted artist biography.
pub const MAX_LASTFM_BIOGRAPHY_BYTES: usize = 256 * 1024;

/// Failure returned while loading or parsing a public `Last.fm` artist wiki.
pub type LastFmError = ProviderError;

/// Full public biography extracted from one artist's `Last.fm` wiki page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LastFmArtistBiography {
    /// Artist name supplied by the caller after whitespace normalization.
    pub artist_name: String,
    /// Validated public `Last.fm` wiki page retained as the biography's source
    /// and attribution link.
    pub wiki_url: Url,
    /// Full wiki body converted to bounded plain text.
    pub description: String,
}

trait LastFmTransport: Send + Sync {
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<LastFmFetchedPage, LastFmError>;
}

#[derive(Debug)]
struct LastFmFetchedPage {
    final_url: Url,
    body: Vec<u8>,
}

#[derive(Clone)]
struct UreqLastFmTransport {
    agent: ureq::Agent,
    timeout: Duration,
}

impl UreqLastFmTransport {
    fn new(timeout: Duration) -> Self {
        Self {
            agent: provider_agent(timeout),
            timeout,
        }
    }
}

impl LastFmTransport for UreqLastFmTransport {
    fn fetch(&self, url: &Url, max_bytes: usize) -> Result<LastFmFetchedPage, LastFmError> {
        let started = Instant::now();
        let mut current_url = url.clone();
        for redirects in 0..=MAX_REDIRECTS {
            validate_lastfm_wiki_url(&current_url).map_err(|_| {
                ProviderError::InvalidResponse(
                    "Last.fm redirected outside its public artist wiki".to_owned(),
                )
            })?;
            let timeout = remaining_request_timeout(self.timeout, started.elapsed())?;
            let mut response = self
                .agent
                .get(current_url.as_str())
                .config()
                .https_only(true)
                .max_redirects(0)
                .timeout_global(Some(timeout))
                .build()
                .header("Accept", "text/html,application/xhtml+xml;q=0.9")
                .header("Accept-Language", "en-US,en;q=0.9")
                .call()
                .map_err(map_ureq_error)?;

            if response.status().is_redirection() {
                if redirects == MAX_REDIRECTS {
                    return Err(ProviderError::InvalidResponse(format!(
                        "Last.fm returned more than {MAX_REDIRECTS} redirects"
                    )));
                }
                let location = response
                    .headers()
                    .get("location")
                    .ok_or_else(|| {
                        ProviderError::InvalidResponse(
                            "Last.fm redirect omitted its Location header".to_owned(),
                        )
                    })?
                    .to_str()
                    .map_err(|_| {
                        ProviderError::InvalidResponse(
                            "Last.fm redirect contained a non-text Location header".to_owned(),
                        )
                    })?;
                let next_url = current_url.join(location).map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "Last.fm returned an invalid redirect URL: {error}"
                    ))
                })?;
                validate_lastfm_wiki_url(&next_url).map_err(|_| {
                    ProviderError::InvalidResponse(
                        "Last.fm redirected outside its public artist wiki".to_owned(),
                    )
                })?;
                current_url = next_url;
                continue;
            }

            if !response.status().is_success() {
                return Err(ProviderError::HttpStatus(response.status().as_u16()));
            }
            validate_html_content_type(response.headers().get("content-type"))?;
            if response
                .body()
                .content_length()
                .is_some_and(|length| length > max_bytes as u64)
            {
                return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
            }
            let bytes = response
                .body_mut()
                .with_config()
                .limit(u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX))
                .read_to_vec()
                .map_err(|error| map_body_error(error, max_bytes))?;
            if bytes.len() > max_bytes {
                return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
            }
            return Ok(LastFmFetchedPage {
                final_url: current_url,
                body: bytes,
            });
        }
        unreachable!("the bounded redirect loop returns on every terminal response")
    }
}

fn remaining_request_timeout(total: Duration, elapsed: Duration) -> Result<Duration, LastFmError> {
    total
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| ProviderError::Transport("Last.fm request timed out".to_owned()))
}

/// Blocking client for public `Last.fm` artist wiki pages.
#[derive(Clone)]
pub struct LastFmProvider {
    transport: Arc<dyn LastFmTransport>,
}

impl LastFmProvider {
    /// Creates a credential-free client with conservative request bounds.
    #[must_use]
    pub fn new() -> Self {
        Self {
            transport: Arc::new(UreqLastFmTransport::new(DEFAULT_REQUEST_TIMEOUT)),
        }
    }

    /// Loads the full public wiki body for `artist_name`, when one exists.
    ///
    /// # Errors
    ///
    /// Returns [`LastFmError`] for invalid input, transport or HTTP failures,
    /// unsafe redirects, oversized responses, malformed UTF-8, or malformed
    /// wiki markup.
    pub fn artist_biography(
        &self,
        artist_name: &str,
    ) -> Result<Option<LastFmArtistBiography>, LastFmError> {
        let artist_name = normalize_artist_name(artist_name)?;
        let wiki_url = artist_wiki_url(&artist_name)?;
        let page = match self.transport.fetch(&wiki_url, MAX_LASTFM_WIKI_HTML_BYTES) {
            Ok(page) => page,
            Err(ProviderError::HttpStatus(404)) => return Ok(None),
            Err(error) => return Err(error),
        };
        if page.body.len() > MAX_LASTFM_WIKI_HTML_BYTES {
            return Err(ProviderError::ResponseTooLarge {
                limit: MAX_LASTFM_WIKI_HTML_BYTES,
            });
        }
        validate_lastfm_wiki_url(&page.final_url).map_err(|_| {
            ProviderError::InvalidResponse(
                "Last.fm redirected outside its public artist wiki".to_owned(),
            )
        })?;
        let html = std::str::from_utf8(&page.body).map_err(|error| {
            ProviderError::InvalidResponse(format!("Last.fm wiki is not UTF-8: {error}"))
        })?;
        parse_artist_wiki_html(&artist_name, &page.final_url, html)
    }
}

impl Default for LastFmProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Parses a previously fetched public artist wiki page into bounded plain text.
///
/// # Errors
///
/// Returns [`LastFmError`] when the artist or URL is invalid, the HTML exceeds
/// its safety bound, or a discovered wiki container is malformed.
pub fn parse_artist_wiki_html(
    artist_name: &str,
    wiki_url: &Url,
    html: &str,
) -> Result<Option<LastFmArtistBiography>, LastFmError> {
    let artist_name = normalize_artist_name(artist_name)?;
    validate_lastfm_wiki_url(wiki_url)?;
    if html.len() > MAX_LASTFM_WIKI_HTML_BYTES {
        return Err(ProviderError::ResponseTooLarge {
            limit: MAX_LASTFM_WIKI_HTML_BYTES,
        });
    }

    let wiki_url = canonical_wiki_url(html, wiki_url)?.unwrap_or_else(|| wiki_url.clone());
    let bodies = full_wiki_bodies(html)?;
    let mut longest = None::<String>;
    for body in bodies {
        let description = wiki_html_to_plain_text(body)?;
        if description.is_empty() {
            continue;
        }
        if longest
            .as_ref()
            .is_none_or(|current| description.len() > current.len())
        {
            longest = Some(description);
        }
    }
    Ok(longest.map(|description| LastFmArtistBiography {
        artist_name,
        wiki_url,
        description,
    }))
}

fn artist_wiki_url(artist_name: &str) -> Result<Url, LastFmError> {
    let mut url = Url::parse(LASTFM_BASE_URL)
        .map_err(|error| ProviderError::InvalidBaseUrl(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| {
            ProviderError::InvalidBaseUrl(
                "Last.fm base URL cannot accept endpoint paths".to_owned(),
            )
        })?
        .push("music")
        .push(artist_name)
        .push("+wiki");
    validate_lastfm_wiki_url(&url)?;
    Ok(url)
}

fn normalize_artist_name(artist_name: &str) -> Result<String, LastFmError> {
    let artist_name = artist_name.trim();
    if artist_name.is_empty()
        || artist_name.len() > MAX_ARTIST_NAME_BYTES
        || artist_name.chars().count() > MAX_ARTIST_NAME_CHARS
        || artist_name.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidRequest(
            "Last.fm artist name must be nonempty, printable, and bounded".to_owned(),
        ));
    }
    Ok(artist_name.to_owned())
}

fn validate_lastfm_wiki_url(url: &Url) -> Result<(), LastFmError> {
    if url.scheme() != "https" {
        return Err(invalid_wiki_url("scheme must be HTTPS"));
    }
    if url.host_str() != Some("www.last.fm") {
        return Err(invalid_wiki_url("host must be www.last.fm"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid_wiki_url("embedded credentials are not allowed"));
    }
    if url.port().is_some() {
        return Err(invalid_wiki_url("ports are not allowed"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid_wiki_url("queries and fragments are not allowed"));
    }
    let segments = url
        .path_segments()
        .ok_or_else(|| invalid_wiki_url("path cannot be segmented"))?
        .collect::<Vec<_>>();
    if segments.len() != 3 || segments[0] != "music" || segments[2] != "+wiki" {
        return Err(invalid_wiki_url("path must be /music/{artist}/+wiki"));
    }
    let artist = percent_decode_path_segment(segments[1])?;
    normalize_artist_name(&artist)?;
    Ok(())
}

fn invalid_wiki_url(message: &str) -> LastFmError {
    ProviderError::InvalidRequest(format!("invalid Last.fm artist wiki URL: {message}"))
}

fn percent_decode_path_segment(segment: &str) -> Result<String, LastFmError> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'%' {
            let high = bytes.get(cursor + 1).and_then(|value| hex_digit(*value));
            let low = bytes.get(cursor + 2).and_then(|value| hex_digit(*value));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(invalid_wiki_url("artist path has invalid percent encoding"));
            };
            decoded.push((high << 4) | low);
            cursor += 3;
        } else {
            decoded.push(bytes[cursor]);
            cursor += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| invalid_wiki_url("artist path is not valid UTF-8"))
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn canonical_wiki_url(html: &str, response_url: &Url) -> Result<Option<Url>, LastFmError> {
    let mut cursor = 0;
    let mut canonical = None::<Url>;
    while let Some(tag) = next_html_tag(html, cursor)? {
        cursor = tag.end;
        if !tag.closing && is_raw_text_tag(tag.name) {
            cursor = skip_raw_text_element(html, tag.end, tag.name)?;
            continue;
        }
        if tag.closing || !tag.name.eq_ignore_ascii_case("link") {
            continue;
        }
        if !tag_attribute(tag.raw, "rel").is_some_and(|value| {
            value
                .split_ascii_whitespace()
                .any(|token| token.eq_ignore_ascii_case("canonical"))
        }) {
            continue;
        }
        let href = tag_attribute(tag.raw, "href").ok_or_else(|| {
            ProviderError::InvalidResponse("Last.fm canonical link omitted its href".to_owned())
        })?;
        let parsed = response_url.join(href.trim()).map_err(|error| {
            ProviderError::InvalidResponse(format!("Last.fm canonical link is invalid: {error}"))
        })?;
        validate_lastfm_wiki_url(&parsed).map_err(|_| {
            ProviderError::InvalidResponse(
                "Last.fm canonical link is not a public artist wiki".to_owned(),
            )
        })?;
        if canonical.as_ref().is_some_and(|current| current != &parsed) {
            return Err(ProviderError::InvalidResponse(
                "Last.fm page contains conflicting canonical wiki links".to_owned(),
            ));
        }
        canonical = Some(parsed);
    }
    Ok(canonical)
}

#[derive(Clone, Copy)]
struct HtmlTag<'a> {
    raw: &'a str,
    name: &'a str,
    start: usize,
    end: usize,
    closing: bool,
    self_closing: bool,
}

#[derive(Clone, Copy)]
struct OpenHtmlTag<'a> {
    name: &'a str,
    is_wiki_container: bool,
}

fn full_wiki_bodies(html: &str) -> Result<Vec<&str>, LastFmError> {
    let mut bodies = Vec::new();
    let mut stack = Vec::<OpenHtmlTag<'_>>::new();
    let mut cursor = 0;
    while let Some(tag) = next_html_tag(html, cursor)? {
        cursor = tag.end;
        if tag.name.is_empty() {
            continue;
        }
        if tag.closing {
            if let Some(index) = stack
                .iter()
                .rposition(|open| open.name.eq_ignore_ascii_case(tag.name))
            {
                stack.truncate(index);
            }
            continue;
        }
        if is_raw_text_tag(tag.name) {
            cursor = skip_raw_text_element(html, tag.end, tag.name)?;
            continue;
        }

        let is_full_wiki = tag.name.eq_ignore_ascii_case("div")
            && stack.last().is_some_and(|parent| parent.is_wiki_container)
            && tag_attribute(tag.raw, "class")
                .is_some_and(|classes| class_has_token(classes, "wiki-content"))
            && tag_attribute(tag.raw, "itemprop")
                .is_some_and(|value| value.eq_ignore_ascii_case("description"));
        if is_full_wiki {
            let close = matching_close_tag(html, tag.end, tag.name)?;
            bodies.push(&html[tag.end..close.start]);
            if bodies.len() > MAX_WIKI_CONTAINERS {
                return Err(ProviderError::InvalidResponse(
                    "Last.fm page contains too many full wiki containers".to_owned(),
                ));
            }
        }

        if !tag.self_closing && !is_void_html_tag(tag.name) {
            if stack.len() >= MAX_HTML_NESTING {
                return Err(ProviderError::InvalidResponse(
                    "Last.fm page HTML nesting is too deep".to_owned(),
                ));
            }
            stack.push(OpenHtmlTag {
                name: tag.name,
                is_wiki_container: tag.name.eq_ignore_ascii_case("div")
                    && tag_attribute(tag.raw, "class")
                        .is_some_and(|classes| class_has_token(classes, "wiki")),
            });
        }
    }
    Ok(bodies)
}

fn matching_close_tag<'a>(
    html: &'a str,
    mut cursor: usize,
    element_name: &str,
) -> Result<HtmlTag<'a>, LastFmError> {
    let mut depth = 1_usize;
    while let Some(tag) = next_html_tag(html, cursor)? {
        cursor = tag.end;
        if tag.name.is_empty() {
            continue;
        }
        if !tag.closing && is_raw_text_tag(tag.name) {
            cursor = skip_raw_text_element(html, tag.end, tag.name)?;
            continue;
        }
        if !tag.name.eq_ignore_ascii_case(element_name) {
            continue;
        }
        if tag.closing {
            depth -= 1;
            if depth == 0 {
                return Ok(tag);
            }
        } else if !tag.self_closing {
            depth = depth.checked_add(1).ok_or_else(|| {
                ProviderError::InvalidResponse("Last.fm wiki nesting overflowed".to_owned())
            })?;
            if depth > MAX_HTML_NESTING {
                return Err(ProviderError::InvalidResponse(
                    "Last.fm wiki nesting is too deep".to_owned(),
                ));
            }
        }
    }
    Err(ProviderError::InvalidResponse(
        "Last.fm full wiki container has no matching closing tag".to_owned(),
    ))
}

fn wiki_html_to_plain_text(html: &str) -> Result<String, LastFmError> {
    let mut output = String::with_capacity(html.len().min(MAX_LASTFM_BIOGRAPHY_BYTES));
    let mut pending_space = false;
    let mut cursor = 0;
    while cursor < html.len() {
        let Some(relative_start) = html[cursor..].find('<') else {
            append_html_text(&mut output, &mut pending_space, &html[cursor..])?;
            break;
        };
        let start = cursor + relative_start;
        append_html_text(&mut output, &mut pending_space, &html[cursor..start])?;
        let Some(tag) = next_html_tag(html, start)? else {
            append_html_text(&mut output, &mut pending_space, &html[start..])?;
            break;
        };
        cursor = tag.end;
        if tag.name.is_empty() {
            continue;
        }
        if !tag.closing
            && (is_raw_text_tag(tag.name)
                || tag_attribute(tag.raw, "class").is_some_and(|classes| {
                    class_has_token(classes, "wiki-last-updated")
                        || class_has_token(classes, "wiki-footer")
                }))
        {
            cursor = skip_raw_text_element(html, tag.end, tag.name)?;
            continue;
        }
        if is_line_break_tag(tag.name) {
            append_line_break(&mut output, &mut pending_space);
        }
    }
    while output.ends_with(char::is_whitespace) {
        output.pop();
    }
    if output.len() > MAX_LASTFM_BIOGRAPHY_BYTES {
        return Err(ProviderError::InvalidResponse(format!(
            "Last.fm biography exceeds {MAX_LASTFM_BIOGRAPHY_BYTES} bytes"
        )));
    }
    Ok(output)
}

fn append_html_text(
    output: &mut String,
    pending_space: &mut bool,
    raw: &str,
) -> Result<(), LastFmError> {
    let decoded = decode_html_entities(raw);
    for character in decoded.chars() {
        if character.is_whitespace() {
            *pending_space = !output.is_empty() && !output.ends_with('\n');
            continue;
        }
        if character.is_control() {
            return Err(ProviderError::InvalidResponse(
                "Last.fm biography contains terminal control characters".to_owned(),
            ));
        }
        if *pending_space && !output.ends_with([' ', '\n']) {
            output.push(' ');
        }
        output.push(character);
        *pending_space = false;
        if output.len() > MAX_LASTFM_BIOGRAPHY_BYTES {
            return Err(ProviderError::InvalidResponse(format!(
                "Last.fm biography exceeds {MAX_LASTFM_BIOGRAPHY_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn append_line_break(output: &mut String, pending_space: &mut bool) {
    while output.ends_with(' ') {
        output.pop();
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    *pending_space = false;
}

fn decode_html_entities(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(relative_ampersand) = value[cursor..].find('&') {
        let ampersand = cursor + relative_ampersand;
        output.push_str(&value[cursor..ampersand]);
        let remainder = &value[ampersand + 1..];
        let semicolon = remainder.find(';').filter(|length| *length <= 32);
        let Some(length) = semicolon else {
            output.push('&');
            cursor = ampersand + 1;
            continue;
        };
        let entity = &remainder[..length];
        if let Some(decoded) = decode_html_entity(entity) {
            output.push(decoded);
        } else {
            output.push('&');
            output.push_str(entity);
            output.push(';');
        }
        cursor = ampersand + length + 2;
    }
    output.push_str(&value[cursor..]);
    output
}

fn decode_html_entity(entity: &str) -> Option<char> {
    let numeric = entity.strip_prefix('#').and_then(|value| {
        if let Some(hex) = value.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            value.parse::<u32>().ok()
        }
    });
    if let Some(value) = numeric {
        return char::from_u32(value);
    }
    match entity {
        "amp" => Some('&'),
        "apos" | "#39" => Some('\''),
        "bull" => Some('•'),
        "copy" => Some('©'),
        "emsp" => Some(' '),
        "ensp" => Some(' '),
        "gt" => Some('>'),
        "hellip" => Some('…'),
        "laquo" => Some('«'),
        "ldquo" => Some('“'),
        "lsquo" => Some('‘'),
        "lt" => Some('<'),
        "mdash" => Some('—'),
        "middot" => Some('·'),
        "nbsp" => Some(' '),
        "ndash" => Some('–'),
        "quot" => Some('"'),
        "raquo" => Some('»'),
        "rdquo" => Some('”'),
        "reg" => Some('®'),
        "rsquo" => Some('’'),
        "thinsp" => Some(' '),
        "trade" => Some('™'),
        _ => None,
    }
}

fn next_html_tag(html: &str, cursor: usize) -> Result<Option<HtmlTag<'_>>, LastFmError> {
    let Some(relative_start) = html[cursor..].find('<') else {
        return Ok(None);
    };
    let start = cursor + relative_start;
    if html[start..].starts_with("<!--") {
        let Some(relative_end) = html[start + 4..].find("-->") else {
            return Err(ProviderError::InvalidResponse(
                "Last.fm page contains an unterminated HTML comment".to_owned(),
            ));
        };
        let end = start + 4 + relative_end + 3;
        return Ok(Some(HtmlTag {
            raw: &html[start..end],
            name: "",
            start,
            end,
            closing: false,
            self_closing: true,
        }));
    }
    let end = html_tag_end(html, start)?;
    let raw = &html[start..end];
    let mut content = raw[1..raw.len() - 1].trim();
    let closing = content.starts_with('/');
    if closing {
        content = content[1..].trim_start();
    }
    let name_length = content
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'))
        .count();
    let name = &content[..name_length];
    Ok(Some(HtmlTag {
        raw,
        name,
        start,
        end,
        closing,
        self_closing: content.trim_end().ends_with('/'),
    }))
}

fn html_tag_end(html: &str, start: usize) -> Result<usize, LastFmError> {
    let bytes = html.as_bytes();
    let mut quote = None::<u8>;
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            byte @ (b'\'' | b'"') if quote.is_none() => quote = Some(byte),
            byte if quote == Some(byte) => quote = None,
            b'>' if quote.is_none() => return Ok(cursor + 1),
            _ => {}
        }
        cursor += 1;
    }
    Err(ProviderError::InvalidResponse(
        "Last.fm page contains an unterminated HTML tag".to_owned(),
    ))
}

fn tag_attribute<'a>(tag: &'a str, wanted: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut index = 1;
    if bytes.get(index) == Some(&b'/') {
        index += 1;
    }
    while index < bytes.len()
        && !bytes[index].is_ascii_whitespace()
        && !matches!(bytes[index], b'>' | b'/')
    {
        index += 1;
    }
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || matches!(bytes[index], b'>' | b'/') {
            break;
        }
        let name_start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'-' | b'_' | b':'))
        {
            index += 1;
        }
        if name_start == index {
            index += 1;
            continue;
        }
        let name = &tag[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'=') {
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let quote = bytes.get(index).copied();
        let (value_start, value_end) = if matches!(quote, Some(b'"' | b'\'')) {
            index += 1;
            let start = index;
            while index < bytes.len() && Some(bytes[index]) != quote {
                index += 1;
            }
            (start, index)
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'>'
            {
                index += 1;
            }
            (start, index)
        };
        if name.eq_ignore_ascii_case(wanted) {
            return tag.get(value_start..value_end);
        }
        if matches!(quote, Some(b'"' | b'\'')) && index < bytes.len() {
            index += 1;
        }
    }
    None
}

fn class_has_token(classes: &str, expected: &str) -> bool {
    classes
        .split_ascii_whitespace()
        .any(|token| token == expected)
}

fn skip_raw_text_element(
    html: &str,
    cursor: usize,
    element_name: &str,
) -> Result<usize, LastFmError> {
    let closing = format!("</{element_name}");
    let relative_start =
        find_ascii_case_insensitive(&html[cursor..], &closing).ok_or_else(|| {
            ProviderError::InvalidResponse(format!(
                "Last.fm page has an unclosed {element_name} element"
            ))
        })?;
    let close_start = cursor + relative_start;
    html_tag_end(html, close_start)
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn is_raw_text_tag(name: &str) -> bool {
    matches_ignore_ascii_case(name, &["script", "style", "template", "noscript"])
}

fn is_line_break_tag(name: &str) -> bool {
    matches_ignore_ascii_case(
        name,
        &[
            "address",
            "article",
            "aside",
            "blockquote",
            "br",
            "dd",
            "div",
            "dl",
            "dt",
            "figcaption",
            "figure",
            "footer",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "header",
            "hr",
            "li",
            "main",
            "nav",
            "ol",
            "p",
            "pre",
            "section",
            "table",
            "tbody",
            "td",
            "tfoot",
            "th",
            "thead",
            "tr",
            "ul",
        ],
    )
}

fn is_void_html_tag(name: &str) -> bool {
    matches_ignore_ascii_case(
        name,
        &[
            "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
            "source", "track", "wbr",
        ],
    )
}

fn matches_ignore_ascii_case(value: &str, choices: &[&str]) -> bool {
    choices
        .iter()
        .any(|choice| value.eq_ignore_ascii_case(choice))
}

fn validate_html_content_type(value: Option<&ureq::http::HeaderValue>) -> Result<(), LastFmError> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = value.to_str().map_err(|_| {
        ProviderError::InvalidResponse("Last.fm returned an invalid Content-Type".to_owned())
    })?;
    let media_type = value.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("text/html")
        || media_type.eq_ignore_ascii_case("application/xhtml+xml")
    {
        Ok(())
    } else {
        Err(ProviderError::InvalidResponse(format!(
            "Last.fm returned non-HTML content type {media_type}"
        )))
    }
}

fn map_ureq_error(error: ureq::Error) -> LastFmError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

fn map_body_error(error: ureq::Error, limit: usize) -> LastFmError {
    match error {
        ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge { limit },
        other => ProviderError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const ARTIST: &str = "тема+креста";
    const RUSSIAN_DESCRIPTION: &str = "самая конфликтная, самая нищебродская и самая сексистская группа.\nновейший дип-хоп - местами абстракт хип-хоп\nкалька на весь бомонд авангардного хип-хопа с примесью женской страдальческой эстетики";
    const FULL_WIKI_FIXTURE: &str = r#"
<!doctype html>
<html>
  <head><meta name="description" content="This is only the short summary."></head>
  <body>
    <section class="wiki-summary">This is only the visible summary.</section>
    <div class="wiki">
      <div data-kind="artist" itemprop="description" class="panel wiki-content prose">
        <p>самая конфликтная, самая нищебродская и самая сексистская группа.</p>
        <p>новейший <em>дип-хоп</em> - местами абстракт хип-хоп</p>
        <p>калька на весь бомонд авангардного хип-хопа с примесью женской страдальческой эстетики</p>
        <p class="wiki-last-updated">Last updated on 1 January 1970.</p>
      </div>
    </div>
  </body>
</html>
"#;

    #[derive(Debug)]
    struct MockTransport {
        response: Mutex<Option<Result<LastFmFetchedPage, LastFmError>>>,
        requests: Mutex<Vec<(Url, usize)>>,
    }

    impl MockTransport {
        fn html(final_url: Url, html: &str) -> Self {
            Self {
                response: Mutex::new(Some(Ok(LastFmFetchedPage {
                    final_url,
                    body: html.as_bytes().to_vec(),
                }))),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl LastFmTransport for MockTransport {
        fn fetch(&self, url: &Url, max_bytes: usize) -> Result<LastFmFetchedPage, LastFmError> {
            self.requests
                .lock()
                .expect("mock requests")
                .push((url.clone(), max_bytes));
            self.response
                .lock()
                .expect("mock response")
                .take()
                .expect("one mock response")
        }
    }

    #[test]
    fn fixture_returns_the_full_russian_wiki_instead_of_the_summary() {
        let wiki_url = fixture_wiki_url();
        let biography = parse_artist_wiki_html(ARTIST, &wiki_url, FULL_WIKI_FIXTURE)
            .expect("fixture should parse")
            .expect("fixture should contain a biography");

        assert_eq!(biography.artist_name, ARTIST);
        assert_eq!(biography.wiki_url, wiki_url);
        assert_eq!(biography.description, RUSSIAN_DESCRIPTION);
        assert!(!biography.description.contains("short summary"));
    }

    #[test]
    fn provider_requests_the_credential_free_artist_wiki_endpoint() {
        let wiki_url = fixture_wiki_url();
        let transport = Arc::new(MockTransport::html(wiki_url.clone(), FULL_WIKI_FIXTURE));
        let provider = LastFmProvider {
            transport: transport.clone(),
        };

        let biography = provider
            .artist_biography(ARTIST)
            .expect("mock fetch should succeed")
            .expect("fixture should contain a biography");

        assert_eq!(biography.description, RUSSIAN_DESCRIPTION);
        let requests = transport.requests.lock().expect("mock requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, wiki_url);
        assert_eq!(requests[0].1, MAX_LASTFM_WIKI_HTML_BYTES);
        assert!(requests[0].0.as_str().ends_with("/+wiki"));
        assert!(requests[0].0.query().is_none());
    }

    #[test]
    fn redirect_requests_share_one_end_to_end_timeout_budget() {
        let total = Duration::from_secs(15);

        assert_eq!(
            remaining_request_timeout(total, Duration::from_secs(4))
                .expect("part of the request budget remains"),
            Duration::from_secs(11)
        );
        assert!(matches!(
            remaining_request_timeout(total, total),
            Err(ProviderError::Transport(message)) if message == "Last.fm request timed out"
        ));
        assert!(matches!(
            remaining_request_timeout(total, Duration::from_secs(16)),
            Err(ProviderError::Transport(message)) if message == "Last.fm request timed out"
        ));
    }

    #[test]
    fn artist_identifier_is_one_encoded_path_segment_and_keeps_literal_plus() {
        assert_eq!(
            artist_wiki_url(ARTIST).expect("fixture artist URL"),
            fixture_wiki_url()
        );
        assert_eq!(
            artist_wiki_url("AC/DC")
                .expect("slash-bearing artist URL")
                .as_str(),
            "https://www.last.fm/music/AC%2FDC/+wiki"
        );
    }

    #[test]
    fn parser_handles_nested_markup_breaks_and_numeric_or_named_entities() {
        let html = r#"
          <div class="wiki">
            <div class='wiki-content' itemprop='description'>
              <p>Rock &amp; roll&nbsp;&mdash; &#x41;&#1041; &unknown;</p>
              <p>Nested <strong>bold <i>text</i></strong><br>next line<!-- hidden --> after comment</p>
              <script>ignored &amp; text</script><style>.ignored {}</style>
            </div>
          </div>
        "#;

        let biography = parse_artist_wiki_html("Artist", &ascii_wiki_url(), html)
            .expect("fixture should parse")
            .expect("fixture should contain a biography");

        assert_eq!(
            biography.description,
            "Rock & roll — AБ &unknown;\nNested bold text\nnext line after comment"
        );
    }

    #[test]
    fn safe_canonical_wiki_url_becomes_the_attribution_link() {
        let html = r#"
          <link href="https://www.last.fm/music/Canonical+artist/+wiki" rel="alternate canonical">
          <div class="wiki"><div class="wiki-content" itemprop="description"><p>Body.</p></div></div>
        "#;

        let biography = parse_artist_wiki_html("Artist", &ascii_wiki_url(), html)
            .expect("fixture should parse")
            .expect("fixture should contain a biography");

        assert_eq!(
            biography.wiki_url.as_str(),
            "https://www.last.fm/music/Canonical+artist/+wiki"
        );
    }

    #[test]
    fn parser_uses_the_longest_full_wiki_container() {
        let html = r#"
          <div class="wiki-content" itemprop="description"><p>False summary.</p></div>
          <div class="wiki">
            <div class="wiki-content" itemprop="description"><p>Short summary.</p></div>
          </div>
          <div class="wiki">
            <div class="wiki-content" itemprop="description"><p>Full first paragraph.</p><p>Full second paragraph.</p></div>
          </div>
        "#;

        let biography = parse_artist_wiki_html("Artist", &ascii_wiki_url(), html)
            .expect("fixture should parse")
            .expect("fixture should contain a biography");

        assert_eq!(
            biography.description,
            "Full first paragraph.\nFull second paragraph."
        );
    }

    #[test]
    fn parser_returns_none_when_the_page_has_no_full_wiki_container() {
        assert_eq!(
            parse_artist_wiki_html(
                "Artist",
                &ascii_wiki_url(),
                "<meta name='description' content='Only a summary'>"
            )
            .expect("missing wiki is not malformed"),
            None
        );
    }

    #[test]
    fn parser_rejects_non_lastfm_or_non_wiki_urls() {
        for raw in [
            "http://www.last.fm/music/Artist/+wiki",
            "https://attacker.example/music/Artist/+wiki",
            "https://www.last.fm/music/Artist",
            "https://www.last.fm/music/Artist/+wiki?token=secret",
            "https://user:secret@www.last.fm/music/Artist/+wiki",
        ] {
            let url = Url::parse(raw).expect("fixture URL");
            assert!(matches!(
                parse_artist_wiki_html("Artist", &url, FULL_WIKI_FIXTURE),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn provider_treats_not_found_as_an_absent_biography() {
        let transport = Arc::new(MockTransport {
            response: Mutex::new(Some(Err(ProviderError::HttpStatus(404)))),
            requests: Mutex::new(Vec::new()),
        });
        let provider = LastFmProvider { transport };

        assert_eq!(
            provider
                .artist_biography("Unknown artist")
                .expect("404 should mean no biography"),
            None
        );
    }

    #[test]
    fn provider_rejects_an_unsafe_final_redirect_without_parsing_it() {
        let transport = Arc::new(MockTransport::html(
            Url::parse("https://attacker.example/music/Artist/+wiki").expect("fixture URL"),
            FULL_WIKI_FIXTURE,
        ));
        let provider = LastFmProvider { transport };

        assert!(matches!(
            provider.artist_biography("Artist"),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn parser_rejects_oversized_pages_and_biographies_instead_of_truncating() {
        let oversized_html = "x".repeat(MAX_LASTFM_WIKI_HTML_BYTES + 1);
        assert!(matches!(
            parse_artist_wiki_html("Artist", &ascii_wiki_url(), &oversized_html),
            Err(ProviderError::ResponseTooLarge {
                limit: MAX_LASTFM_WIKI_HTML_BYTES
            })
        ));

        let oversized_body = "x".repeat(MAX_LASTFM_BIOGRAPHY_BYTES + 1);
        let html = format!(
            "<div class='wiki'><div class='wiki-content' itemprop='description'><p>{oversized_body}</p></div></div>"
        );
        assert!(matches!(
            parse_artist_wiki_html("Artist", &ascii_wiki_url(), &html),
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn provider_rejects_empty_control_bearing_or_oversized_artist_names() {
        let transport = Arc::new(MockTransport {
            response: Mutex::new(Some(Err(ProviderError::Unsupported))),
            requests: Mutex::new(Vec::new()),
        });
        let provider = LastFmProvider { transport };

        for artist in ["", "   ", "line\nbreak"] {
            assert!(matches!(
                provider.artist_biography(artist),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
        let oversized = "a".repeat(1_025);
        assert!(matches!(
            provider.artist_biography(&oversized),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    fn fixture_wiki_url() -> Url {
        Url::parse(
            "https://www.last.fm/music/%D1%82%D0%B5%D0%BC%D0%B0+%D0%BA%D1%80%D0%B5%D1%81%D1%82%D0%B0/+wiki",
        )
        .expect("fixture URL")
    }

    fn ascii_wiki_url() -> Url {
        Url::parse("https://www.last.fm/music/Artist/+wiki").expect("fixture URL")
    }
}
