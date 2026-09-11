//! Bounded, credential-free browsing of HTTP directory pages and direct media.
//!
//! HTML supplies links, never filesystem paths or executable content. Only the
//! selected page is fetched; discovery does not crawl folders, probe each file,
//! download audio, or run a site extractor. The controller owns navigation and
//! rejects stale asynchronous listings independently of this transport.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use html5gum::{DefaultEmitter, Token, Tokenizer};
use percent_encoding::percent_decode_str;
use url::Url;

use crate::domain::MediaKind;
use crate::local_browser::{LocalEntryKind, classify_local_file};

const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_URL_BYTES: usize = 16 * 1024;
const MAX_NAME_BYTES: usize = 512;
const MAX_INSPECTED_LINKS: usize = 10_000;
const MAX_VISIBLE_ENTRIES: usize = 1_000;
const MAX_REDIRECTS: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Navigation or playback behavior for one link in a Web listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebEntryKind {
    /// A linked HTTP directory, fetched only after explicit navigation.
    Directory,
    /// A directly linked audio file or explicit HTML audio source.
    Audio,
    /// A directly linked video container, played by Youta as audio only.
    Video,
}

/// One validated link with a bounded, terminal-safe filename.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebEntry {
    /// Exact HTTP(S) target, preserving encoded path segments and query values.
    pub url: Url,
    /// Display-only basename; never used to reconstruct the request URL.
    pub name: String,
    /// Whether activation navigates to a directory or starts audio playback.
    pub kind: WebEntryKind,
}

impl WebEntry {
    /// Returns the existing media type for playable entries, or `None` for folders.
    #[must_use]
    pub const fn media_kind(&self) -> Option<MediaKind> {
        match self.kind {
            WebEntryKind::Directory => None,
            WebEntryKind::Audio => Some(MediaKind::Audio),
            WebEntryKind::Video => Some(MediaKind::Video),
        }
    }
}

/// An immutable, bounded snapshot of one fetched page or explicit media URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebDirectoryListing {
    /// Final validated page URL after redirects, without a fragment.
    pub url: Url,
    /// Parent navigation target, presented separately from child entries.
    pub parent: Option<Url>,
    /// Deduplicated links, sorted by filename with directories before media.
    pub entries: Vec<WebEntry>,
    /// Whether the inspected-link or visible-entry limit stopped discovery.
    pub truncated: bool,
}

/// Safe-to-display failures that never include request URLs or query secrets.
#[derive(Debug, thiserror::Error)]
pub enum WebBrowserError {
    /// Unsupported, credential-bearing, hostless, or oversized input URL.
    #[error("Web address must be a credential-free HTTP or HTTPS URL of at most 16 KiB")]
    InvalidUrl,
    /// A network connection or HTTP transfer failed.
    #[error("Could not fetch the Web address; check the address and connection")]
    Transport,
    /// Redirects and body transfer exhausted their shared time budget.
    #[error("Web directory request timed out")]
    TimedOut,
    /// The server returned an unsuccessful HTTP status.
    #[error("Web server returned HTTP {0}")]
    HttpStatus(u16),
    /// A redirect omitted its location or pointed to an unsupported target.
    #[error("Web server returned an invalid or unsafe redirect")]
    InvalidRedirect,
    /// The redirect chain exceeded its fixed bound.
    #[error("Web server exceeded the three-redirect limit")]
    TooManyRedirects,
    /// A directory body exceeded its decoded byte limit.
    #[error("Web directory response exceeds the 2 MiB limit")]
    ResponseTooLarge,
    /// The server returned neither HTML nor an explicit audio/video type.
    #[error("Web address did not return an HTML directory or supported audio/video")]
    UnsupportedContentType,
}

/// Validates a Web URL without making a request.
///
/// Private and loopback addresses are intentionally supported for local HTTP
/// servers. Credentials and non-HTTP schemes are never accepted.
///
/// # Errors
///
/// Returns [`WebBrowserError::InvalidUrl`] for unsafe or unsupported inputs.
pub fn validate_web_url(url: &Url) -> Result<(), WebBrowserError> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.as_str().len() > MAX_URL_BYTES
        || url.as_str().chars().any(char::is_control)
    {
        return Err(WebBrowserError::InvalidUrl);
    }
    Ok(())
}

/// Loads one Web listing without importing browser or provider credentials.
#[derive(Clone, Debug)]
pub struct WebBrowserClient {
    timeout: Duration,
}

impl Default for WebBrowserClient {
    fn default() -> Self {
        Self {
            timeout: REQUEST_TIMEOUT,
        }
    }
}

impl WebBrowserClient {
    /// Lists a selected HTTP page or returns one directly recognized media URL.
    ///
    /// Direct media is not downloaded or probed during discovery. HTML fetches
    /// use a decoded-body bound and one deadline shared across all redirects.
    ///
    /// # Errors
    ///
    /// Returns [`WebBrowserError`] for URL, transport, status, redirect, type,
    /// or response-size failures. Discovery never returns unbounded output.
    pub fn list(&self, url: &Url) -> Result<WebDirectoryListing, WebBrowserError> {
        self.list_with_transport(url, &UreqWebTransport, Instant::now)
    }

    /// Keeps redirects and deadline handling testable without an external server.
    fn list_with_transport(
        &self,
        url: &Url,
        transport: &impl WebTransport,
        mut now: impl FnMut() -> Instant,
    ) -> Result<WebDirectoryListing, WebBrowserError> {
        validate_web_url(url)?;
        let mut current = without_fragment(url);
        let deadline = now() + self.timeout;
        for redirects in 0..=MAX_REDIRECTS {
            if let Some(kind) = media_kind_from_url(&current, None) {
                return Ok(direct_listing(current, kind));
            }
            let remaining = deadline.saturating_duration_since(now());
            if remaining.is_zero() {
                return Err(WebBrowserError::TimedOut);
            }
            let response = transport.fetch(&current, remaining)?;
            if now() >= deadline {
                return Err(WebBrowserError::TimedOut);
            }
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                if redirects == MAX_REDIRECTS {
                    return Err(WebBrowserError::TooManyRedirects);
                }
                let location = response.location.ok_or(WebBrowserError::InvalidRedirect)?;
                let next = current
                    .join(&location)
                    .map_err(|_| WebBrowserError::InvalidRedirect)?;
                validate_web_url(&next).map_err(|_| WebBrowserError::InvalidRedirect)?;
                current = without_fragment(&next);
                continue;
            }
            if !(200..300).contains(&response.status) {
                return Err(WebBrowserError::HttpStatus(response.status));
            }
            if let Some(kind) = media_kind_from_content_type(response.content_type.as_deref()) {
                return Ok(direct_listing(current, kind));
            }
            if !is_html_content_type(response.content_type.as_deref()) {
                return Err(WebBrowserError::UnsupportedContentType);
            }
            return parse_web_directory(&current, &response.body);
        }
        Err(WebBrowserError::TooManyRedirects)
    }
}

/// One HTTP hop; injected responses are revalidated by the client.
trait WebTransport {
    fn fetch(&self, url: &Url, timeout: Duration) -> Result<WebResponse, WebBrowserError>;
}

/// Only metadata needed for validated redirect handling and HTML parsing.
struct WebResponse {
    status: u16,
    location: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// A fresh per-hop agent cannot replay cookies set by an earlier response.
struct UreqWebTransport;

impl WebTransport for UreqWebTransport {
    fn fetch(&self, url: &Url, timeout: Duration) -> Result<WebResponse, WebBrowserError> {
        validate_web_url(url)?;
        // Automatic redirects would bypass validation. Reusing an agent could
        // also replay Set-Cookie when another feature enables ureq's cookie jar.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .proxy(None)
            .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let mut response = agent
            .get(url.as_str())
            .header(
                "Accept",
                "text/html,application/xhtml+xml,audio/*;q=0.9,video/*;q=0.9",
            )
            .call()
            .map_err(map_transport_error)?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .map(|value| value.to_str().map(str::to_owned))
            .transpose()
            .map_err(|_| WebBrowserError::InvalidRedirect)?;
        let content_type = response
            .headers()
            .get("content-type")
            .map(|value| value.to_str().map(str::to_owned))
            .transpose()
            .map_err(|_| WebBrowserError::UnsupportedContentType)?;
        let body = if (200..300).contains(&status)
            && media_kind_from_content_type(content_type.as_deref()).is_none()
            && is_html_content_type(content_type.as_deref())
        {
            if response
                .body()
                .content_length()
                .is_some_and(|length| length > MAX_HTML_BYTES as u64)
            {
                return Err(WebBrowserError::ResponseTooLarge);
            }
            let bytes = response
                .body_mut()
                .with_config()
                .limit((MAX_HTML_BYTES + 1) as u64)
                .read_to_vec()
                .map_err(map_transport_error)?;
            if bytes.len() > MAX_HTML_BYTES {
                return Err(WebBrowserError::ResponseTooLarge);
            }
            bytes
        } else {
            // Redirect/error bodies are irrelevant. An audio/video response
            // supplies only its type; discovery must not download the media.
            Vec::new()
        };
        Ok(WebResponse {
            status,
            location,
            content_type,
            body,
        })
    }
}

/// Maps HTTP failures without retaining URL-bearing error strings.
fn map_transport_error(error: ureq::Error) -> WebBrowserError {
    match error {
        ureq::Error::Timeout(_) => WebBrowserError::TimedOut,
        ureq::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            WebBrowserError::TimedOut
        }
        ureq::Error::BodyExceedsLimit(_) => WebBrowserError::ResponseTooLarge,
        ureq::Error::StatusCode(status) => WebBrowserError::HttpStatus(status),
        _ => WebBrowserError::Transport,
    }
}

/// Extracts supported links from one bounded HTML document without network I/O.
///
/// Uses HTML tokenization for attributes and entities; comments, script/style
/// raw text, and template contents cannot manufacture media links. The final
/// response URL (not the original pre-redirect address) is the default base.
///
/// # Errors
///
/// Returns [`WebBrowserError::InvalidUrl`] for the base address or
/// [`WebBrowserError::ResponseTooLarge`] for oversized HTML.
pub fn parse_web_directory(url: &Url, html: &[u8]) -> Result<WebDirectoryListing, WebBrowserError> {
    parse_web_directory_with_limits(url, html, MAX_INSPECTED_LINKS, MAX_VISIBLE_ENTRIES)
}

/// Applies independently bounded inspection and display budgets during parsing.
#[allow(
    clippy::too_many_lines,
    reason = "one ordered, bounded token pass keeps base, template, and media context together"
)]
fn parse_web_directory_with_limits(
    url: &Url,
    html: &[u8],
    max_links: usize,
    max_entries: usize,
) -> Result<WebDirectoryListing, WebBrowserError> {
    validate_web_url(url)?;
    if html.len() > MAX_HTML_BYTES {
        return Err(WebBrowserError::ResponseTooLarge);
    }
    let url = without_fragment(url);
    let parent = parent_url(&url);
    let mut base = url.clone();
    let mut has_base = false;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    let mut inspected = 0_usize;
    let mut truncated = false;
    let mut template_depth = 0_usize;
    let mut media_context = None;
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    for token in Tokenizer::new_with_emitter(html, emitter) {
        let Ok(token) = token;
        let tag = match token {
            Token::StartTag(tag) => tag,
            Token::EndTag(tag) => {
                if tag.name.as_slice() == b"template" {
                    template_depth = template_depth.saturating_sub(1);
                } else if template_depth == 0 && matches!(tag.name.as_slice(), b"audio" | b"video")
                {
                    media_context = None;
                }
                continue;
            }
            _ => continue,
        };
        if tag.name.as_slice() == b"template" {
            template_depth = template_depth.saturating_add(1);
            continue;
        }
        if template_depth > 0 {
            continue;
        }
        let attribute = |name: &[u8]| {
            tag.attributes
                .get(name)
                .and_then(|value| std::str::from_utf8(value.value.as_ref()).ok())
        };
        if !has_base && tag.name.as_slice() == b"base" {
            if let Some(candidate) = attribute(b"href").and_then(|href| url.join(href).ok())
                && validate_web_url(&candidate).is_ok()
            {
                base = without_fragment(&candidate);
                has_base = true;
            }
            continue;
        }
        let (target, hint) = match tag.name.as_slice() {
            b"a" => (attribute(b"href"), None),
            b"audio" => {
                media_context = Some(WebEntryKind::Audio);
                (attribute(b"src"), media_context)
            }
            b"video" => {
                media_context = Some(WebEntryKind::Video);
                (attribute(b"src"), media_context)
            }
            b"source" => (
                attribute(b"src"),
                media_kind_from_content_type(attribute(b"type")).or(media_context),
            ),
            _ => continue,
        };
        let Some(target) = target else {
            continue;
        };
        inspected = inspected.saturating_add(1);
        if inspected > max_links {
            truncated = true;
            break;
        }
        let target = target.trim();
        if target.is_empty() || target.starts_with(['?', '#']) {
            continue;
        }
        let Some(candidate) = base
            .join(target)
            .ok()
            .filter(|candidate| validate_web_url(candidate).is_ok())
        else {
            continue;
        };
        let candidate = without_fragment(&candidate);
        let kind = if let Some(kind) = media_kind_from_url(&candidate, hint) {
            kind
        } else if hint.is_none() && candidate.path().ends_with('/') {
            if same_location(&candidate, &url)
                || parent
                    .as_ref()
                    .is_some_and(|parent| same_location(&candidate, parent))
            {
                continue;
            }
            WebEntryKind::Directory
        } else {
            continue;
        };
        if !seen.insert(candidate.clone()) {
            continue;
        }
        if entries.len() == max_entries {
            truncated = true;
            break;
        }
        entries.push(WebEntry {
            name: url_display_name(&candidate),
            url: candidate,
            kind,
        });
    }
    entries.sort_by_cached_key(|entry| {
        (
            entry.kind != WebEntryKind::Directory,
            entry.name.to_lowercase(),
            entry.url.as_str().to_owned(),
        )
    });
    Ok(WebDirectoryListing {
        url,
        parent,
        entries,
        truncated,
    })
}

/// Identifies known media extensions, using explicit HTML media types as fallback.
fn media_kind_from_url(url: &Url, hint: Option<WebEntryKind>) -> Option<WebEntryKind> {
    let segment = url.path_segments()?.next_back()?;
    let name = percent_decode_str(segment).decode_utf8_lossy();
    match classify_local_file(Path::new(name.as_ref())) {
        Some(LocalEntryKind::Audio) => Some(WebEntryKind::Audio),
        Some(LocalEntryKind::Video) => Some(WebEntryKind::Video),
        Some(_) => None,
        None => hint,
    }
}

/// Reads an explicit media MIME type without treating generic binary as audio.
fn media_kind_from_content_type(content_type: Option<&str>) -> Option<WebEntryKind> {
    let mime = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    if mime.starts_with("audio/") || mime == "application/ogg" {
        Some(WebEntryKind::Audio)
    } else if mime.starts_with("video/") {
        Some(WebEntryKind::Video)
    } else {
        None
    }
}

/// Missing MIME metadata is tolerated, but explicit non-HTML bodies are rejected.
fn is_html_content_type(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|value| {
        let mime = value.split(';').next().unwrap_or_default().trim();
        mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("application/xhtml+xml")
    })
}

/// Removes browser-only fragments while retaining request-significant encoding.
fn without_fragment(url: &Url) -> Url {
    let mut url = url.clone();
    url.set_fragment(None);
    url
}

/// Compares directory locations independently of sorting-query parameters.
fn same_location(left: &Url, right: &Url) -> bool {
    left.origin() == right.origin() && left.path() == right.path()
}

/// Computes one syntactic HTTP parent without inheriting query credentials.
fn parent_url(url: &Url) -> Option<Url> {
    if url.path() == "/" {
        return None;
    }
    let mut parent = url
        .join(if url.path().ends_with('/') { ".." } else { "." })
        .ok()?;
    parent.set_query(None);
    parent.set_fragment(None);
    Some(parent)
}

/// Creates a one-item listing without requesting any bytes from a media resource.
fn direct_listing(url: Url, kind: WebEntryKind) -> WebDirectoryListing {
    WebDirectoryListing {
        parent: parent_url(&url),
        entries: vec![WebEntry {
            name: url_display_name(&url),
            url: url.clone(),
            kind,
        }],
        url,
        truncated: false,
    }
}

/// Decodes a display-only filename and removes terminal and bidi control bytes.
fn url_display_name(url: &Url) -> String {
    let basename = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .unwrap_or_else(|| url.host_str().unwrap_or("Web media"));
    let decoded = percent_decode_str(basename).decode_utf8_lossy();
    let mut name = String::new();
    for character in decoded.chars().filter(|character| {
        !character.is_control()
            && !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        if name.len() + character.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        name.push(character);
    }
    let name = name.trim();
    if name.is_empty() {
        "Web media".to_owned()
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    fn page() -> Url {
        Url::parse("http://192.168.100.2:8000/books/").unwrap()
    }

    #[test]
    fn web_urls_accept_lan_http_and_reject_non_http_or_credentials() {
        for value in [
            "http://192.168.100.2:8000/books/",
            "http://localhost:8000/",
            "http://[::1]:8000/",
            "https://example.org/music/?page=2",
        ] {
            validate_web_url(&Url::parse(value).unwrap()).expect("valid Web URL");
        }
        for value in [
            "file:///tmp/music/",
            "ftp://example.org/music/",
            "javascript:alert(1)",
            "data:text/html,hello",
            "http://user@example.org/",
            "http://user:password@example.org/",
        ] {
            assert!(
                validate_web_url(&Url::parse(value).unwrap()).is_err(),
                "{value}"
            );
        }
        let oversized = Url::parse(&format!(
            "https://example.org/{}",
            "x".repeat(MAX_URL_BYTES)
        ))
        .unwrap();
        assert!(validate_web_url(&oversized).is_err());
    }

    #[test]
    fn python_directory_listing_preserves_urls_and_sorts_folders_before_media() {
        let listing = parse_web_directory(
            &page(),
            br##"<!DOCTYPE HTML>
            <a href="../">Parent Directory</a>
            <a href="z-track.MP3">z-track.MP3</a>
            <a href="cover.jpg">cover.jpg</a>
            <a href="Chapter%201%20%26%20intro.opus">Chapter 1 &amp; intro.opus</a>
            <a href="nested%20folder/">nested folder/</a>
            <a href="a-video.MP4?download=1&amp;quality=best#ignored">video</a>
            <a href="./z-track.MP3#duplicate">duplicate</a>
            <a href="?C=N;O=D">Name</a><a href="#top">Top</a>
            <a href="./">Current directory</a><a href="notes.txt">notes</a>
        "##,
        )
        .unwrap();
        assert_eq!(listing.url, page());
        assert_eq!(
            listing.parent.unwrap().as_str(),
            "http://192.168.100.2:8000/"
        );
        assert!(!listing.truncated);
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            [
                "nested folder",
                "a-video.MP4",
                "Chapter 1 & intro.opus",
                "z-track.MP3"
            ]
        );
        assert_eq!(listing.entries[0].kind, WebEntryKind::Directory);
        assert_eq!(listing.entries[1].kind, WebEntryKind::Video);
        assert_eq!(
            listing.entries[1].url.as_str(),
            "http://192.168.100.2:8000/books/a-video.MP4?download=1&quality=best"
        );
        assert_eq!(
            listing.entries[2].url.path(),
            "/books/Chapter%201%20%26%20intro.opus"
        );
        assert_eq!(
            listing.entries[2].media_kind(),
            Some(crate::domain::MediaKind::Audio)
        );
        assert_eq!(listing.entries[0].media_kind(), None);
    }

    #[test]
    fn web_directory_understands_media_elements_entities_and_unquoted_attributes() {
        let listing = parse_web_directory(
            &page(),
            br"
            <audio src=first.opus></audio>
            <video src='second.webm'></video>
            <audio><source src='stream?id=1&amp;part=2' type='audio/ogg'></audio>
            <source src='picture.png' type='image/png'>
            <A HREF='third&#46;MP3'>play</A>
            <audio src='extensionless'></audio>
            <video><source src='muxed' type='video/mp4'></video>
        ",
        )
        .unwrap();
        assert_eq!(listing.entries.len(), 6);
        assert!(
            listing
                .entries
                .iter()
                .any(|entry| entry.url.query() == Some("id=1&part=2")
                    && entry.kind == WebEntryKind::Audio)
        );
        assert!(
            listing
                .entries
                .iter()
                .any(|entry| entry.name == "muxed" && entry.kind == WebEntryKind::Video)
        );
        assert!(
            listing
                .entries
                .iter()
                .any(|entry| entry.name == "third.MP3")
        );
    }

    #[test]
    fn web_directory_ignores_raw_text_comments_templates_and_unsafe_targets() {
        let listing = parse_web_directory(
            &page(),
            br#"
            <!-- <a href='comment.mp3'>comment</a> -->
            <script>const fake = "<a href='script.mp3'>fake</a>";</script>
            <style>p::after { content: "<a href='style.mp3'>fake</a>" }</style>
            <textarea><a href='textarea.mp3'>fake</a></textarea>
            <title><a href='title.mp3'>fake</a></title>
            <template><template><a href='template.mp3'>fake</a></template></template>
            <a href='javascript:evil.mp3'>bad</a><a href='file:///tmp/local.mp3'>bad</a>
            <a href='https://user:password@example.org/private.mp3'>bad</a>
            <a href='//cdn.example.org/public.opus'>good</a>
        "#,
        )
        .unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(
            listing.entries[0].url.as_str(),
            "http://cdn.example.org/public.opus"
        );
    }

    #[test]
    fn web_directory_uses_the_first_valid_base_and_preserves_encoded_path_segments() {
        let listing = parse_web_directory(
            &page(),
            br"
            <base href='javascript:invalid'>
            <base href='/music/subdirectory/'>
            <base href='/ignored/'>
            <a href='../%D0%A2%D0%B5%D1%81%D1%82%20%23%3F.mp3'>label</a>
            <a href='album/'>album</a>
        ",
        )
        .unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.entries[1].name, "Тест #?.mp3");
        assert_eq!(
            listing.entries[1].url.path(),
            "/music/%D0%A2%D0%B5%D1%81%D1%82%20%23%3F.mp3"
        );
        assert_eq!(listing.entries[1].url.query(), None);
        assert_eq!(listing.url, page());
    }

    #[test]
    fn web_entry_labels_strip_terminal_controls_and_have_a_utf8_safe_limit() {
        let html = format!(
            "<a href='bad%1B%5B31m%0Aname.mp3'>bad</a><a href='{}.opus'>long</a>",
            "Ж".repeat(MAX_NAME_BYTES)
        );
        let listing = parse_web_directory(&page(), html.as_bytes()).unwrap();
        assert_eq!(listing.entries.len(), 2);
        for entry in listing.entries {
            assert!(entry.name.len() <= MAX_NAME_BYTES);
            assert!(!entry.name.chars().any(char::is_control));
            assert!(!entry.name.is_empty());
        }
    }

    #[test]
    fn web_directory_bounds_body_links_and_visible_entries() {
        assert!(matches!(
            parse_web_directory(&page(), &vec![b'x'; MAX_HTML_BYTES + 1]),
            Err(WebBrowserError::ResponseTooLarge)
        ));
        let html = b"<a href='a.mp3'>a</a><a href='b.mp3'>b</a><a href='c.mp3'>c</a>";
        let listing = parse_web_directory_with_limits(&page(), html, 10, 2).unwrap();
        assert!(listing.truncated);
        assert_eq!(listing.entries.len(), 2);
        let html = b"<a href='a.txt'>a</a><a href='b.txt'>b</a><a href='c.mp3'>c</a>";
        let listing = parse_web_directory_with_limits(&page(), html, 2, 10).unwrap();
        assert!(listing.truncated);
        assert!(listing.entries.is_empty());
    }

    #[test]
    fn web_parent_navigation_stops_at_the_origin_root() {
        let listing =
            parse_web_directory(&Url::parse("http://localhost:8000/").unwrap(), b"").unwrap();
        assert!(listing.parent.is_none());
        let listing = parse_web_directory(
            &Url::parse("http://localhost:8000/a/b/?sort=name").unwrap(),
            b"",
        )
        .unwrap();
        assert_eq!(listing.parent.unwrap().as_str(), "http://localhost:8000/a/");
    }

    #[derive(Default)]
    struct MockTransport {
        responses: RefCell<VecDeque<WebResponse>>,
        requests: RefCell<Vec<(Url, Duration)>>,
    }

    impl WebTransport for MockTransport {
        fn fetch(&self, url: &Url, timeout: Duration) -> Result<WebResponse, WebBrowserError> {
            self.requests.borrow_mut().push((url.clone(), timeout));
            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .expect("unexpected HTTP request"))
        }
    }

    fn html_response(body: &[u8]) -> WebResponse {
        WebResponse {
            status: 200,
            location: None,
            content_type: Some("text/html; charset=utf-8".into()),
            body: body.to_vec(),
        }
    }

    fn redirect(location: &str) -> WebResponse {
        WebResponse {
            status: 301,
            location: Some(location.into()),
            content_type: None,
            body: Vec::new(),
        }
    }

    #[test]
    fn web_client_returns_direct_media_without_fetching_or_probing_it() {
        let url = Url::parse("http://localhost:8000/track.OPUS?download=1#fragment").unwrap();
        let transport = MockTransport::default();
        let listing = WebBrowserClient::default()
            .list_with_transport(&url, &transport, Instant::now)
            .unwrap();
        assert!(transport.requests.borrow().is_empty());
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].kind, WebEntryKind::Audio);
        assert_eq!(listing.entries[0].url.fragment(), None);
        assert_eq!(listing.entries[0].url.query(), Some("download=1"));
        assert_eq!(listing.parent.unwrap().as_str(), "http://localhost:8000/");
    }

    #[test]
    fn web_client_resolves_links_against_the_final_redirect_url() {
        let transport = MockTransport::default();
        transport.responses.borrow_mut().extend([
            redirect("/books/"),
            html_response(b"<a href='track.mp3'>track</a>"),
        ]);
        let initial = Url::parse("http://localhost:8000/books").unwrap();
        let listing = WebBrowserClient::default()
            .list_with_transport(&initial, &transport, Instant::now)
            .unwrap();
        assert_eq!(listing.url.as_str(), "http://localhost:8000/books/");
        assert_eq!(
            listing.entries[0].url.as_str(),
            "http://localhost:8000/books/track.mp3"
        );
        assert_eq!(transport.requests.borrow().len(), 2);
    }

    #[test]
    fn web_client_revalidates_redirects_and_never_fetches_unsafe_locations() {
        for target in ["file:///tmp/listing", "https://user:password@example.org/"] {
            let transport = MockTransport::default();
            transport.responses.borrow_mut().push_back(redirect(target));
            assert!(
                WebBrowserClient::default()
                    .list_with_transport(&page(), &transport, Instant::now)
                    .is_err()
            );
            assert_eq!(transport.requests.borrow().len(), 1);
        }
        let transport = MockTransport::default();
        transport
            .responses
            .borrow_mut()
            .extend((0..=MAX_REDIRECTS).map(|_| redirect("/again/")));
        assert!(matches!(
            WebBrowserClient::default().list_with_transport(&page(), &transport, Instant::now),
            Err(WebBrowserError::TooManyRedirects)
        ));
        assert_eq!(transport.requests.borrow().len(), MAX_REDIRECTS + 1);
    }

    #[test]
    fn web_client_uses_one_overall_timeout_across_redirects() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let transport = MockTransport::default();
        transport
            .responses
            .borrow_mut()
            .push_back(redirect("/again/"));
        let error = WebBrowserClient::default()
            .list_with_transport(&page(), &transport, || {
                let current = clock.get();
                clock.set(current + REQUEST_TIMEOUT / 2);
                current
            })
            .unwrap_err();
        assert!(matches!(error, WebBrowserError::TimedOut));
        assert_eq!(transport.requests.borrow().len(), 1);
        assert_eq!(transport.requests.borrow()[0].1, REQUEST_TIMEOUT / 2);
    }

    #[test]
    fn web_client_rechecks_mocked_status_type_and_body_limits() {
        for (response, expected) in [
            (
                WebResponse {
                    status: 404,
                    ..html_response(b"")
                },
                "status",
            ),
            (
                WebResponse {
                    content_type: Some("application/json".into()),
                    ..html_response(b"{}")
                },
                "type",
            ),
            (html_response(&vec![b'x'; MAX_HTML_BYTES + 1]), "limit"),
            (
                WebResponse {
                    location: None,
                    ..redirect("/ignored/")
                },
                "redirect",
            ),
        ] {
            let transport = MockTransport::default();
            transport.responses.borrow_mut().push_back(response);
            let error = WebBrowserClient::default()
                .list_with_transport(&page(), &transport, Instant::now)
                .unwrap_err();
            assert!(match expected {
                "status" => matches!(error, WebBrowserError::HttpStatus(404)),
                "type" => matches!(error, WebBrowserError::UnsupportedContentType),
                "limit" => matches!(error, WebBrowserError::ResponseTooLarge),
                "redirect" => matches!(error, WebBrowserError::InvalidRedirect),
                _ => false,
            });
        }
    }

    /// Serves a finite HTTP script locally with bounded accepts and request reads.
    fn mock_http_server(responses: Vec<Vec<u8>>) -> (Url, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        use std::net::{Ipv4Addr, TcpListener};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "mock HTTP accept timed out");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("mock HTTP accept failed: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(
                        request.len() < MAX_URL_BYTES + 4096,
                        "oversized mock request"
                    );
                    let mut byte = [0_u8; 1];
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                requests.push(String::from_utf8(request).unwrap());
                // Size/type checks may deliberately close without draining a body.
                let _ = socket.write_all(&response);
            }
            requests
        });
        (
            Url::parse(&format!("http://{address}/listing")).unwrap(),
            worker,
        )
    }

    #[test]
    fn web_http_client_follows_validated_redirects_without_cookie_or_auth_replay() {
        let body = b"<a href='track.mp3'>track</a>";
        let (url, worker) = mock_http_server(vec![
            b"HTTP/1.1 301 Moved Permanently\r\nLocation: /books/\r\nSet-Cookie: session=mock-secret; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), String::from_utf8_lossy(body),
            ).into_bytes(),
        ]);
        let result = WebBrowserClient::default().list(&url);
        let requests = worker.join().unwrap();
        let listing = result.expect("bounded local HTTP listing");
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].url.path(), "/books/track.mp3");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /listing HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("GET /books/ HTTP/1.1\r\n"));
        for request in requests {
            let lower = request.to_ascii_lowercase();
            assert!(!lower.contains("\r\ncookie:"));
            assert!(!lower.contains("\r\nauthorization:"));
        }
    }

    #[test]
    fn web_http_client_bounds_advertised_and_chunked_html_bodies() {
        let advertised = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_HTML_BYTES + 1,
        ).into_bytes();
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            MAX_HTML_BYTES + 1, "x".repeat(MAX_HTML_BYTES + 1),
        ).into_bytes();
        for response in [advertised, chunked] {
            let (url, worker) = mock_http_server(vec![response]);
            let result = WebBrowserClient::default().list(&url);
            worker.join().unwrap();
            assert!(matches!(result, Err(WebBrowserError::ResponseTooLarge)));
        }
    }

    #[test]
    fn web_http_client_recognizes_media_type_without_reading_the_advertised_body() {
        let (url, worker) = mock_http_server(vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: audio/ogg\r\nContent-Length: 10000000000\r\nConnection: close\r\n\r\n".to_vec(),
        ]);
        let result = WebBrowserClient::default().list(&url);
        worker.join().unwrap();
        let listing = result.expect("media discovery must not read the absent body");
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].kind, WebEntryKind::Audio);
        assert_eq!(listing.entries[0].url, url);
    }

    #[test]
    fn web_client_recognizes_extensionless_audio_from_response_type() {
        let transport = MockTransport::default();
        transport.responses.borrow_mut().push_back(WebResponse {
            content_type: Some("audio/ogg".into()),
            ..html_response(b"")
        });
        let listing = WebBrowserClient::default()
            .list_with_transport(&page(), &transport, Instant::now)
            .unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].kind, WebEntryKind::Audio);
    }
}
