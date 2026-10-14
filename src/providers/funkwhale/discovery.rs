//! Bounded, explicit discovery from Funkwhale's official static Join directory.

use std::collections::BTreeSet;

use html5gum::{DefaultEmitter, Token, Tokenizer};
use url::Url;

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError, resolve_http_url, validate_base_url};
use crate::domain::remote_url_has_non_public_host;

/// Official public-pod directory, fetched only when its picker is opened.
pub const PUBLIC_PODS_URL: &str = "https://www.funkwhale.audio/join/";

const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_PUBLIC_PODS: usize = 128;
const MAX_INSPECTED_CARDS: usize = 512;
const MAX_ELEMENT_DEPTH: usize = 128;
const MAX_LABEL_CHARS: usize = 160;

/// One advertised pod without contacting its server or requiring an account.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunkwhalePod {
    /// Credential-free HTTP(S) base URL, including a trailing slash.
    pub url: Url,
    /// Bounded directory label, falling back to the URL's hostname.
    pub label: String,
    /// Directory registration metadata, or `None` when it was not advertised.
    pub open_registrations: Option<bool>,
}

/// Fetches the official public-pod directory with bounded time and response size.
///
/// # Errors
///
/// Returns a provider error for transport, size, or unrecognized directory data.
pub fn discover_public_pods() -> Result<Vec<FunkwhalePod>, ProviderError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(DEFAULT_REQUEST_TIMEOUT))
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let source = Url::parse(PUBLIC_PODS_URL).expect("the fixed Funkwhale directory URL is valid");
    fetch_directory(&agent, &source, MAX_HTML_BYTES)
}

/// Reads one fixed directory response without following redirects or visiting pods.
fn fetch_directory(
    agent: &ureq::Agent,
    source: &Url,
    limit: usize,
) -> Result<Vec<FunkwhalePod>, ProviderError> {
    let response = agent
        .get(source.as_str())
        .header("Accept", "text/html")
        .call()
        .map_err(crate::providers::map_ureq_error)?;
    parse_directory_response(response, limit)
}

/// Applies status and bounded-body policy to real or in-memory HTTP responses.
fn parse_directory_response(
    mut response: ureq::http::Response<ureq::Body>,
    limit: usize,
) -> Result<Vec<FunkwhalePod>, ProviderError> {
    if response.status().as_u16() != 200 {
        return Err(ProviderError::HttpStatus(response.status().as_u16()));
    }
    if response
        .body()
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ProviderError::ResponseTooLarge { limit });
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit.saturating_add(1) as u64)
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge { limit },
            other => crate::providers::map_ureq_error(other),
        })?;
    if bytes.len() > limit {
        return Err(ProviderError::ResponseTooLarge { limit });
    }
    parse_public_pods(&bytes)
}

/// Parses advertised pod cards without fetching links or executing page scripts.
///
/// # Errors
///
/// Returns a provider error when HTML exceeds its byte limit or has no valid pods.
pub fn parse_public_pods(html: &[u8]) -> Result<Vec<FunkwhalePod>, ProviderError> {
    if html.len() > MAX_HTML_BYTES {
        return Err(ProviderError::ResponseTooLarge {
            limit: MAX_HTML_BYTES,
        });
    }
    let source = Url::parse(PUBLIC_PODS_URL).expect("the fixed Funkwhale directory URL is valid");
    let mut state = DirectoryParser::default();
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    for token in Tokenizer::new_with_emitter(html, emitter) {
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) => {
                let hidden = state.hidden();
                let attribute = |name: &[u8]| {
                    tag.attributes
                        .get(name)
                        .and_then(|value| std::str::from_utf8(value.value.as_ref()).ok())
                };
                let classes = attribute(b"class").unwrap_or_default();
                let has_class =
                    |value| classes.split_ascii_whitespace().any(|class| class == value);
                let depth = state.elements.len() + 1;
                if !hidden && tag.name.as_slice() == b"article" && has_class("instances") {
                    state.article_depth = Some(depth);
                }
                if !hidden
                    && state.article_depth.is_some()
                    && state.card.is_none()
                    && tag.name.as_slice() == b"div"
                    && has_class("funkwhale")
                    && has_class("card")
                {
                    state.inspected += 1;
                    if state.inspected > MAX_INSPECTED_CARDS {
                        break;
                    }
                    state.card = Some(PodCard {
                        depth,
                        label: String::new(),
                        url: None,
                        open_registrations: registration(attribute(b"data-category")),
                    });
                }
                if !hidden && let Some(card) = state.card.as_mut() {
                    if tag.name.as_slice() == b"h3" && has_class("card-title") {
                        state.title_depth = Some(depth);
                    }
                    if tag.name.as_slice() == b"a" && card.url.is_none() {
                        card.url = attribute(b"href").and_then(|href| pod_url(&source, href));
                    }
                }
                if !tag.self_closing && !is_void_element(tag.name.as_slice()) {
                    if state.elements.len() == MAX_ELEMENT_DEPTH {
                        return Err(ProviderError::InvalidResponse(
                            "Funkwhale pod directory exceeds HTML nesting limit".to_owned(),
                        ));
                    }
                    state.elements.push(tag.name.to_vec());
                }
            }
            Token::EndTag(tag) => {
                if let Some(index) = state
                    .elements
                    .iter()
                    .rposition(|name| name.as_slice() == tag.name.as_slice())
                {
                    state.elements.truncate(index);
                    state.close_elements(index);
                }
            }
            Token::String(text) if state.title_depth.is_some() && !state.hidden() => {
                if let Some(card) = state.card.as_mut()
                    && card.label.len() < MAX_LABEL_CHARS * 32
                {
                    card.label.extend(
                        String::from_utf8_lossy(&text)
                            .chars()
                            .take(MAX_LABEL_CHARS * 4),
                    );
                }
            }
            _ => {}
        }
        if state.pods.len() == MAX_PUBLIC_PODS {
            break;
        }
    }
    state.close_elements(0);
    if state.pods.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "The official Funkwhale Join page contains no valid public pod cards".to_owned(),
        ));
    }
    Ok(state.pods)
}

/// Small bounded token state for the official article/card/title structure.
#[derive(Default)]
struct DirectoryParser {
    elements: Vec<Vec<u8>>,
    article_depth: Option<usize>,
    title_depth: Option<usize>,
    card: Option<PodCard>,
    pods: Vec<FunkwhalePod>,
    seen: BTreeSet<Url>,
    inspected: usize,
}

impl DirectoryParser {
    /// Templates and raw script/style text never contribute suggestions or labels.
    fn hidden(&self) -> bool {
        self.elements
            .iter()
            .any(|name| matches!(name.as_slice(), b"template" | b"script" | b"style"))
    }

    /// Finalizes a complete or EOF-terminated card and releases closed contexts.
    fn close_elements(&mut self, depth: usize) {
        if self.title_depth.is_some_and(|title| title > depth) {
            self.title_depth = None;
        }
        if self.card.as_ref().is_some_and(|card| card.depth > depth)
            && let Some(card) = self.card.take()
            && let Some(url) = card.url
            && self.pods.len() < MAX_PUBLIC_PODS
            && self.seen.insert(url.clone())
        {
            let clean = card
                .label
                .chars()
                .filter(|character| !character.is_control() || character.is_whitespace())
                .collect::<String>();
            let label = clean
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(MAX_LABEL_CHARS)
                .collect::<String>();
            self.pods.push(FunkwhalePod {
                label: if label.is_empty() {
                    url.host_str().unwrap_or_default().to_owned()
                } else {
                    label
                },
                url,
                open_registrations: card.open_registrations,
            });
        }
        if self.article_depth.is_some_and(|article| article > depth) {
            self.article_depth = None;
        }
    }
}

/// One card awaiting its matching closing element.
struct PodCard {
    depth: usize,
    label: String,
    url: Option<Url>,
    open_registrations: Option<bool>,
}

/// Accepts absolute/protocol-relative public HTTP(S) pod bases, not page navigation.
fn pod_url(source: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    if !href.starts_with("//") && Url::parse(href).is_err() {
        return None;
    }
    let url = validate_base_url(resolve_http_url(source, href).ok()?).ok()?;
    if url.host_str() == source.host_str() || remote_url_has_non_public_host(&url) {
        return None;
    }
    Some(url)
}

/// Retains explicit registration metadata without inferring access to public music.
fn registration(category: Option<&str>) -> Option<bool> {
    let categories = category
        .unwrap_or_default()
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    match (categories.contains(&"open"), categories.contains(&"closed")) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// HTML void elements do not add an unmatched structural nesting level.
fn is_void_element(name: &[u8]) -> bool {
    matches!(
        name,
        b"area"
            | b"base"
            | b"br"
            | b"col"
            | b"embed"
            | b"hr"
            | b"img"
            | b"input"
            | b"link"
            | b"meta"
            | b"param"
            | b"source"
            | b"track"
            | b"wbr"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOIN_FIXTURE: &[u8] = include_bytes!("fixtures/join.html");

    /// Wraps fixtures in the same static article/card structure as the Join page.
    fn directory(cards: &str) -> String {
        format!(
            "<article class='instances'><section><div class='cards'>{cards}</div></section></article>"
        )
    }

    /// Supplies one mocked advertised pod, never a real production pod list.
    fn card(href: &str, label: &str, category: &str) -> String {
        format!(
            "<div class='funkwhale card' data-category='{category}'><h3 class='card-title'>{label}</h3><div class='card-content'><p>10 accounts</p></div><a href='{href}'>Visit pod</a></div>"
        )
    }

    #[test]
    fn official_join_cards_supply_labels_urls_and_registration_metadata() {
        let pods = parse_public_pods(JOIN_FIXTURE).expect("parse mocked Join directory");
        assert_eq!(pods.len(), 3);
        assert_eq!(pods[0].url.as_str(), "https://open.example/");
        assert_eq!(pods[0].label, "Open & Free Music");
        assert_eq!(pods[0].open_registrations, Some(true));
        assert_eq!(pods[1].url.as_str(), "https://closed.example/prefix/");
        assert_eq!(pods[1].label, "Céliglyphe pod");
        assert_eq!(pods[1].open_registrations, Some(false));
        assert_eq!(pods[2].url.as_str(), "http://unknown.example:8080/");
        assert_eq!(pods[2].label, "unknown.example");
        assert_eq!(pods[2].open_registrations, None);
    }

    #[test]
    fn links_outside_cards_and_inert_templates_never_become_pods() {
        let html = format!(
            "<a href='https://navigation.example'>Help</a><div class='funkwhale card'><a href='https://outside.example'>Outside</a></div><article class='instances'><template>{}</template><script>{}</script><style>{}</style>{}</article>",
            card("https://template.example", "Hidden", "open"),
            card("https://script.example", "Hidden", "open"),
            card("https://style.example", "Hidden", "open"),
            card("https://visible.example", "Visible", "closed"),
        );
        let pods = parse_public_pods(html.as_bytes()).expect("visible card");
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].url.host_str(), Some("visible.example"));
    }

    #[test]
    fn unsafe_malformed_and_navigation_urls_are_rejected_before_deduplication() {
        let mut cards = String::new();
        for href in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/plain,hello",
            "https://user:password@credentials.example",
            "https:///",
            "#section",
            "/help",
            "https://www.funkwhale.audio/join/",
            "https://query.example/?token=private",
            "https://fragment.example/#private",
        ] {
            cards.push_str(&card(href, "Untrusted", "open"));
        }
        cards.push_str(&card("https://safe.example", "Safe", "open"));
        let pods = parse_public_pods(directory(&cards).as_bytes()).expect("one valid pod");
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].url.as_str(), "https://safe.example/");
    }

    #[test]
    fn normalized_pod_urls_are_deduplicated_in_directory_order() {
        let html = directory(&format!(
            "{}{}{}",
            card("https://EXAMPLE.test:443", "First label", "open"),
            card("//example.test/", "Duplicate label", "closed"),
            card("https://other.example", "Second label", "closed"),
        ));
        let pods = parse_public_pods(html.as_bytes()).expect("deduplicated pods");
        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].label, "First label");
        assert_eq!(pods[1].label, "Second label");
    }

    #[test]
    fn labels_decode_entities_collapse_whitespace_and_drop_terminal_controls() {
        let html = directory(&card(
            "//pod.example",
            "  Music &#38;\n\t<span>Friends</span> &#x1b; ",
            "all",
        ));
        let pods = parse_public_pods(html.as_bytes()).expect("decoded label");
        assert_eq!(pods[0].label, "Music & Friends");
        assert_eq!(pods[0].open_registrations, None);
    }

    #[test]
    fn labels_and_visible_pod_count_are_bounded() {
        let mut cards = String::new();
        for index in 0..MAX_PUBLIC_PODS + 10 {
            cards.push_str(&card(
                &format!("https://pod-{index}.example"),
                &"音".repeat(500),
                "open",
            ));
        }
        let pods = parse_public_pods(directory(&cards).as_bytes()).expect("bounded directory");
        assert_eq!(pods.len(), MAX_PUBLIC_PODS);
        assert!(pods.iter().all(|pod| pod.label.chars().count() <= 160));
    }

    #[test]
    fn empty_or_unrecognized_directories_report_an_actionable_error() {
        for html in [b"".as_slice(), b"<html><body>Maintenance</body></html>"] {
            assert!(matches!(
                parse_public_pods(html),
                Err(ProviderError::InvalidResponse(_))
            ));
        }
    }

    #[test]
    fn html_larger_than_the_response_budget_is_rejected_before_tokenization() {
        assert!(matches!(
            parse_public_pods(&vec![b'x'; MAX_HTML_BYTES + 1]),
            Err(ProviderError::ResponseTooLarge {
                limit: MAX_HTML_BYTES
            })
        ));
    }

    #[test]
    fn private_or_local_directory_suggestions_are_not_public_pods() {
        let mut cards = String::new();
        for href in [
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://192.168.1.2/",
            "http://localhost/",
            "http://pod.local/",
            "http://pod.internal/",
        ] {
            cards.push_str(&card(href, "Local", "open"));
        }
        cards.push_str(&card("https://public.example/", "Public", "open"));
        let pods = parse_public_pods(directory(&cards).as_bytes()).unwrap();
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].label, "Public");
    }

    #[test]
    fn excessive_nesting_is_rejected_and_inspected_cards_are_bounded() {
        let deep = format!(
            "{}{}",
            "<div>".repeat(MAX_ELEMENT_DEPTH + 1),
            card("https://pod.example", "Pod", "open")
        );
        assert!(matches!(
            parse_public_pods(deep.as_bytes()),
            Err(ProviderError::InvalidResponse(_))
        ));
        let mut cards = card("https://first.example", "First", "open");
        cards.push_str(&card("javascript:alert(1)", "Invalid", "open").repeat(MAX_INSPECTED_CARDS));
        cards.push_str(&card("https://last.example", "Not inspected", "open"));
        let pods = parse_public_pods(directory(&cards).as_bytes()).unwrap();
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].label, "First");
    }

    #[test]
    fn directory_response_parses_static_html_without_visiting_pods() {
        let response = ureq::http::Response::builder()
            .status(200)
            .body(
                ureq::Body::builder()
                    .mime_type("text/html")
                    .data(JOIN_FIXTURE.to_vec()),
            )
            .unwrap();
        assert_eq!(
            parse_directory_response(response, MAX_HTML_BYTES)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn directory_response_bounds_advertised_and_streaming_bodies() {
        for body in [
            ureq::Body::builder().data(vec![b'x'; 128]),
            ureq::Body::builder().reader(std::io::Cursor::new(vec![b'x'; 128])),
        ] {
            let response = ureq::http::Response::builder()
                .status(200)
                .body(body)
                .unwrap();
            assert!(matches!(
                parse_directory_response(response, 64),
                Err(ProviderError::ResponseTooLarge { limit: 64 })
            ));
        }
    }

    #[test]
    fn directory_redirects_and_http_errors_are_not_parsed_as_success() {
        for status in [301, 401, 403, 503] {
            let response = ureq::http::Response::builder()
                .status(status)
                .header("Location", "https://redirect.example/")
                .body(ureq::Body::builder().data(JOIN_FIXTURE.to_vec()))
                .unwrap();
            assert!(
                matches!(parse_directory_response(response, MAX_HTML_BYTES), Err(ProviderError::HttpStatus(code)) if code == status)
            );
        }
    }
}
