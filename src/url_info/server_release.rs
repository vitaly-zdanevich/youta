//! Exact upstream release dates for explicitly reported server versions.
//!
//! These dates do not establish when a server was installed or whether a distro
//! applied security fixes. Only fixed official project sources are requested.

use std::collections::VecDeque;

use html5gum::{DefaultEmitter, Token, Tokenizer};
use serde_json::Value;
use url::Url;

use super::{
    Budget, DocumentKind, Failure, HttpTransport, MAX_FIELD_BYTES, MAX_HTML_TOKENS, check_status,
    parse_json, safe_text,
};

const MAX_CACHE_ENTRIES: usize = 64;
const APACHE_SOURCE: &str = "https://httpd.apache.org/security/vulnerabilities_24.html";
const MONTHS: [(&str, &str); 12] = [
    ("Jan", "January"),
    ("Feb", "February"),
    ("Mar", "March"),
    ("Apr", "April"),
    ("May", "May"),
    ("Jun", "June"),
    ("Jul", "July"),
    ("Aug", "August"),
    ("Sep", "September"),
    ("Oct", "October"),
    ("Nov", "November"),
    ("Dec", "December"),
];

/// Only these products have an exact, bounded official-source parser.
#[derive(Clone, Copy)]
enum Product {
    Nginx,
    Apache,
    Caddy,
}

impl Product {
    fn name(self) -> &'static str {
        match self {
            Self::Nginx => "nginx",
            Self::Apache => "Apache",
            Self::Caddy => "Caddy",
        }
    }

    /// Every host and path prefix is fixed; only validated numeric version segments vary.
    fn source(self, version: &str) -> (String, DocumentKind, String) {
        match self {
            Self::Nginx => {
                let parts: Vec<_> = version.split('.').collect();
                let source = if parts[1]
                    .parse::<u16>()
                    .is_ok_and(|minor| minor.is_multiple_of(2))
                {
                    format!("https://nginx.org/en/CHANGES-{}.{}", parts[0], parts[1])
                } else {
                    "https://nginx.org/en/CHANGES".to_owned()
                };
                (source.clone(), DocumentKind::Text, source)
            }
            Self::Apache => (
                APACHE_SOURCE.to_owned(),
                DocumentKind::Html,
                APACHE_SOURCE.to_owned(),
            ),
            Self::Caddy => (
                format!("https://api.github.com/repos/caddyserver/caddy/releases/tags/v{version}"),
                DocumentKind::Json,
                format!("https://github.com/caddyserver/caddy/releases/tag/v{version}"),
            ),
        }
    }
}

/// A validated Gregorian date preserves day precision when checking conflicting records.
#[derive(Clone, Copy, Eq, PartialEq)]
struct ReleaseDate {
    year: u16,
    month: usize,
    day: u8,
}

impl ReleaseDate {
    fn new(year: u16, month: usize, day: u8) -> Option<Self> {
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let days = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        (1970..=9999).contains(&year).then_some(())?;
        (1..=12).contains(&month).then_some(())?;
        (1..=days[month - 1])
            .contains(&day)
            .then_some(Self { year, month, day })
    }

    fn display(self) -> String {
        format!("{} {}", MONTHS[self.month - 1].1, self.year)
    }
}

/// One exact upstream version and its official publication month, not an age estimate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReleaseInfo {
    pub(super) product: String,
    pub(super) version: String,
    pub(super) released: String,
    pub(super) source: String,
}

/// Keeps a bounded session-only cache without persisting visited URLs or response documents.
#[derive(Default)]
pub(super) struct ReleaseClient {
    cache: VecDeque<(String, Option<ReleaseInfo>)>,
}

impl ReleaseClient {
    /// Queries only a supported exact version; unavailable or unknown dates may be omitted.
    pub(super) fn lookup(
        &mut self,
        header: &str,
        transport: &impl HttpTransport,
        budget: &Budget<'_>,
    ) -> Result<Option<ReleaseInfo>, Failure> {
        let Some((product, version)) = parse_header(header) else {
            return Ok(None);
        };
        budget.remaining()?;
        let key = format!("{}/{version}", product.name());
        if let Some((_, result)) = self.cache.iter().find(|(entry, _)| entry == &key) {
            return Ok(result.clone());
        }
        let (endpoint, kind, source) = product.source(version);
        let url = Url::parse(&endpoint).map_err(|_| Failure::InvalidDocument)?;
        let response = transport.fetch(&url, kind, budget.request()?, budget.cancelled)?;
        budget.remaining()?;
        // An official endpoint may not redirect this lookup to an unrelated service.
        if (300..400).contains(&response.status) {
            return Err(Failure::Redirect);
        }
        let date = if response.status == 404 {
            None
        } else {
            check_status(response.status)?;
            if !kind.accepts(&response.content_type) {
                return Err(Failure::WrongType);
            }
            if response.body.len() > kind.limit() {
                return Err(Failure::TooLarge);
            }
            match product {
                Product::Nginx => nginx_date(&response.body, version)?,
                Product::Apache => apache_date(&response.body, version)?,
                Product::Caddy => caddy_date(&response.body, version)?,
            }
        };
        budget.remaining()?;
        let result = date.map(|date| ReleaseInfo {
            product: product.name().into(),
            version: version.into(),
            released: date.display(),
            source,
        });
        while self.cache.len() >= MAX_CACHE_ENTRIES {
            self.cache.pop_front();
        }
        self.cache.push_back((key, result.clone()));
        Ok(result)
    }
}

/// Accepts an exact three-component upstream version, but not custom builds or prereleases.
fn parse_header(header: &str) -> Option<(Product, &str)> {
    if header.len() > MAX_FIELD_BYTES || header.chars().any(char::is_control) {
        return None;
    }
    let token = header.split_ascii_whitespace().next()?;
    let (name, version) = token.split_once('/')?;
    let product = match name.to_ascii_lowercase().as_str() {
        "nginx" => Product::Nginx,
        "apache" => Product::Apache,
        "caddy" => Product::Caddy,
        _ => return None,
    };
    let version = if matches!(product, Product::Caddy) {
        version.strip_prefix('v').unwrap_or(version)
    } else {
        version
    };
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || !parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 4
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
        })
    {
        return None;
    }
    if matches!(product, Product::Apache) && !version.starts_with("2.4.") {
        return None;
    }
    Some((product, version))
}

/// The changelog header, not an arbitrary date in a release note, identifies nginx publication.
fn nginx_date(bytes: &[u8], version: &str) -> Result<Option<ReleaseDate>, Failure> {
    let text = std::str::from_utf8(bytes).map_err(|_| Failure::InvalidDocument)?;
    let mut found = None;
    for line in text.lines() {
        let parts: Vec<_> = line.split_ascii_whitespace().take(8).collect();
        if parts.len() != 7 || parts[..3] != ["Changes", "with", "nginx"] || parts[3] != version {
            continue;
        }
        let date = parts[4].parse().ok().and_then(|day| {
            let month = MONTHS.iter().position(|(short, _)| *short == parts[5])? + 1;
            ReleaseDate::new(parts[6].parse().ok()?, month, day)
        });
        let Some(date) = date else { return Ok(None) };
        if found.is_some_and(|previous| previous != date) {
            return Ok(None);
        }
        found = Some(date);
    }
    Ok(found)
}

/// Apache's security table distinguishes release dates from vulnerability report/fix dates.
fn apache_date(bytes: &[u8], version: &str) -> Result<Option<ReleaseDate>, Failure> {
    let expected = format!("Update {version} released");
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut cells = Vec::new();
    let mut cell: Option<String> = None;
    let mut hidden = 0_usize;
    let mut found = None;
    for (index, token) in Tokenizer::new_with_emitter(bytes, emitter).enumerate() {
        if index >= MAX_HTML_TOKENS {
            return Err(Failure::TooLarge);
        }
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) => match tag.name.as_slice() {
                b"script" | b"style" | b"template" | b"noscript" => hidden += 1,
                b"tr" if hidden == 0 => {
                    cells.clear();
                    cell = None;
                }
                b"td" if hidden == 0 => cell = Some(String::new()),
                _ => {}
            },
            Token::String(value) if hidden == 0 => {
                if let Some(cell) = cell.as_mut() {
                    if cell.len() + value.value.len() > MAX_FIELD_BYTES {
                        return Err(Failure::TooLarge);
                    }
                    cell.push_str(&String::from_utf8_lossy(value.value.as_ref()));
                }
            }
            Token::EndTag(tag) => match tag.name.as_slice() {
                b"script" | b"style" | b"template" | b"noscript" => {
                    hidden = hidden.saturating_sub(1);
                }
                b"td" if hidden == 0 => {
                    if cells.len() < 3
                        && let Some(cell) = cell.take()
                    {
                        cells.push(safe_text(&cell));
                    }
                }
                b"tr" if hidden == 0 && cells.len() == 2 && cells[0] == expected => {
                    let Some(date) = iso_date(&cells[1]) else {
                        return Ok(None);
                    };
                    if found.is_some_and(|previous| previous != date) {
                        return Ok(None);
                    }
                    found = Some(date);
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(found)
}

/// A matching public GitHub tag's publication timestamp is distinct from commit creation time.
fn caddy_date(bytes: &[u8], version: &str) -> Result<Option<ReleaseDate>, Failure> {
    let value = parse_json(bytes)?;
    if value.get("tag_name").and_then(Value::as_str) != Some(format!("v{version}").as_str())
        || value.get("draft").and_then(Value::as_bool) != Some(false)
        || value.get("prerelease").and_then(Value::as_bool) != Some(false)
    {
        return Ok(None);
    }
    let Some(timestamp) = value.get("published_at").and_then(Value::as_str) else {
        return Ok(None);
    };
    let bytes = timestamp.as_bytes();
    if bytes.len() != 20
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
        || ![11..13, 14..16, 17..19]
            .iter()
            .all(|range| bytes[range.clone()].iter().all(u8::is_ascii_digit))
        || timestamp[11..13]
            .parse::<u8>()
            .ok()
            .is_none_or(|hour| hour > 23)
        || timestamp[14..16]
            .parse::<u8>()
            .ok()
            .is_none_or(|minute| minute > 59)
        || timestamp[17..19]
            .parse::<u8>()
            .ok()
            .is_none_or(|second| second > 59)
    {
        return Ok(None);
    }
    Ok(iso_date(&timestamp[..10]))
}

/// Validates full Gregorian dates before reducing their precision for display.
fn iso_date(value: &str) -> Option<ReleaseDate> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || ![0..4, 5..7, 8..10]
            .iter()
            .all(|range| bytes[range.clone()].iter().all(u8::is_ascii_digit))
    {
        return None;
    }
    ReleaseDate::new(
        value[..4].parse().ok()?,
        value[5..7].parse().ok()?,
        value[8..].parse().ok()?,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Mutex, atomic::AtomicBool};
    use std::time::{Duration, Instant};

    use url::Url;

    use super::super::{DocumentKind, HttpResponse};
    use super::*;

    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<VecDeque<HttpResponse>>,
        requests: Mutex<Vec<String>>,
    }

    impl MockTransport {
        fn push(&self, content_type: &str, body: &str) {
            self.responses.lock().unwrap().push_back(HttpResponse {
                status: 200,
                content_type: content_type.into(),
                body: body.as_bytes().to_vec(),
                ..HttpResponse::default()
            });
        }
    }

    impl HttpTransport for MockTransport {
        fn fetch(
            &self,
            url: &Url,
            _kind: DocumentKind,
            _timeout: Duration,
            _cancelled: &AtomicBool,
        ) -> Result<HttpResponse, Failure> {
            self.requests.lock().unwrap().push(url.to_string());
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request"))
        }
    }

    #[test]
    fn nginx_exact_mainline_and_stable_versions_use_official_branch_dates() {
        for (header, version, date, released, source) in [
            (
                "nginx/1.31.6",
                "1.31.6",
                "15 Sep 2026",
                "September 2026",
                "https://nginx.org/en/CHANGES",
            ),
            (
                "nginx/1.24.0 (Ubuntu)",
                "1.24.0",
                "11 Apr 2023",
                "April 2023",
                "https://nginx.org/en/CHANGES-1.24",
            ),
        ] {
            let transport = MockTransport::default();
            transport.push(
				"text/plain",
				&format!(
					"Changes with nginx 1.31.60 01 Jan 1999\nChanges with nginx {version}    {date}\n"
				),
			);
            let cancelled = AtomicBool::new(false);
            let info = ReleaseClient::default()
                .lookup(header, &transport, &Budget::new(&cancelled))
                .unwrap()
                .unwrap();
            assert_eq!(info.product, "nginx");
            assert_eq!(info.version, version);
            assert_eq!(info.released, released);
            assert_eq!(info.source, source);
            assert_eq!(*transport.requests.lock().unwrap(), [source]);
        }
    }

    #[test]
    fn apache_uses_only_the_exact_released_table_row_not_report_or_fix_dates() {
        let transport = MockTransport::default();
        transport.push("text/html", "<table><tr><td>Reported to security team</td><td>2024-06-20</td></tr><tr><td>fixed by r1919249 in 2.4.x</td><td>2024-07-15</td></tr><tr><td>Update 2.4.62 released</td><td>2024-07-17</td></tr><tr><td>Update 2.4.620 released</td><td>2025-08-01</td></tr></table>");
        let cancelled = AtomicBool::new(false);
        let result = ReleaseClient::default()
            .lookup(
                "Apache/2.4.62 (Debian)",
                &transport,
                &Budget::new(&cancelled),
            )
            .unwrap()
            .unwrap();
        assert_eq!(result.released, "July 2024");
        assert_eq!(
            result.source,
            "https://httpd.apache.org/security/vulnerabilities_24.html"
        );
    }

    #[test]
    fn caddy_requires_exact_tag_public_stable_release_and_publication_timestamp() {
        let cancelled = AtomicBool::new(false);
        let transport = MockTransport::default();
        transport.push("application/json", r#"{"tag_name":"v2.9.1","draft":false,"prerelease":false,"created_at":"2024-12-01T00:00:00Z","published_at":"2025-01-08T15:22:53Z"}"#);
        let result = ReleaseClient::default()
            .lookup("Caddy/v2.9.1", &transport, &Budget::new(&cancelled))
            .unwrap()
            .unwrap();
        assert_eq!(result.released, "January 2025");
        assert_eq!(
            result.source,
            "https://github.com/caddyserver/caddy/releases/tag/v2.9.1"
        );
        assert_eq!(
            *transport.requests.lock().unwrap(),
            ["https://api.github.com/repos/caddyserver/caddy/releases/tags/v2.9.1"]
        );
        for json in [
            r#"{"tag_name":"v2.9.10","draft":false,"prerelease":false,"published_at":"2025-01-08T15:22:53Z"}"#,
            r#"{"tag_name":"v2.9.1","draft":true,"prerelease":false,"published_at":"2025-01-08T15:22:53Z"}"#,
            r#"{"tag_name":"v2.9.1","draft":false,"prerelease":true,"published_at":"2025-01-08T15:22:53Z"}"#,
            r#"{"tag_name":"v2.9.1","draft":false,"prerelease":false,"published_at":"2025-02-29T15:22:53Z"}"#,
            r#"{"tag_name":"v2.9.1","draft":false,"prerelease":false,"published_at":"2025-01-08T99:22:53Z"}"#,
        ] {
            transport.push("application/json", json);
            assert!(
                ReleaseClient::default()
                    .lookup("Caddy/2.9.1", &transport, &Budget::new(&cancelled))
                    .unwrap()
                    .is_none(),
                "{json}"
            );
        }
    }

    #[test]
    fn unsupported_or_custom_headers_do_not_trigger_any_network_request() {
        let cancelled = AtomicBool::new(false);
        let transport = MockTransport::default();
        let mut client = ReleaseClient::default();
        for header in [
            "",
            "Caddy",
            "Apache",
            "nginx",
            "cloudflare",
            "Apache/2.2.34",
            "nginx/1.2",
            "nginx/1.2.3-custom",
            "nginx/1.02.3",
            "Caddy/2.9.1+build",
            "nginx/1.2.3/evil",
            "nginx/1.2.3\nSecret: token",
            "other/1.2.3",
            "nginx/1.2.3?secret=x",
        ] {
            assert!(
                client
                    .lookup(header, &transport, &Budget::new(&cancelled))
                    .unwrap()
                    .is_none(),
                "{header}"
            );
        }
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn confirmed_dates_and_unknown_versions_are_cached_with_a_bounded_session_limit() {
        let cancelled = AtomicBool::new(false);
        let transport = MockTransport::default();
        transport.push("text/plain", "Changes with nginx 1.31.6 15 Sep 2026\n");
        let mut client = ReleaseClient::default();
        let first = client
            .lookup("nginx/1.31.6", &transport, &Budget::new(&cancelled))
            .unwrap();
        assert!(first.is_some());
        assert_eq!(
            client
                .lookup(
                    "nginx/1.31.6 (Ubuntu)",
                    &transport,
                    &Budget::new(&cancelled)
                )
                .unwrap(),
            first
        );
        for version in 0..70 {
            transport.push("text/plain", "No exact release here\n");
            let header = format!("nginx/1.31.{version}");
            if version == 6 {
                continue;
            }
            assert!(
                client
                    .lookup(&header, &transport, &Budget::new(&cancelled))
                    .unwrap()
                    .is_none()
            );
        }
        let before = transport.requests.lock().unwrap().len();
        assert!(
            client
                .lookup("nginx/1.31.69", &transport, &Budget::new(&cancelled))
                .unwrap()
                .is_none()
        );
        assert_eq!(transport.requests.lock().unwrap().len(), before);
        assert!(client.cache.len() <= 64);
    }

    #[test]
    fn redirect_mime_size_cancellation_and_deadline_are_enforced_without_referrals() {
        let cancelled = AtomicBool::new(false);
        let transport = MockTransport::default();
        transport.responses.lock().unwrap().push_back(HttpResponse {
            status: 302,
            location: Some("https://tracker.example/secret".into()),
            ..HttpResponse::default()
        });
        assert_eq!(
            ReleaseClient::default().lookup("nginx/1.31.6", &transport, &Budget::new(&cancelled)),
            Err(Failure::Redirect)
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        transport.push("text/html", "Changes with nginx 1.31.6 15 Sep 2026");
        assert_eq!(
            ReleaseClient::default().lookup("nginx/1.31.6", &transport, &Budget::new(&cancelled)),
            Err(Failure::WrongType)
        );
        transport.push("text/plain", &"x".repeat(DocumentKind::Text.limit() + 1));
        assert_eq!(
            ReleaseClient::default().lookup("nginx/1.31.6", &transport, &Budget::new(&cancelled)),
            Err(Failure::TooLarge)
        );
        let before = transport.requests.lock().unwrap().len();
        assert_eq!(
            ReleaseClient::default().lookup(
                "nginx/1.31.6",
                &transport,
                &Budget::new(&AtomicBool::new(true))
            ),
            Err(Failure::Cancelled)
        );
        let budget = Budget {
            deadline: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            ..Budget::new(&cancelled)
        };
        assert_eq!(
            ReleaseClient::default().lookup("nginx/1.31.6", &transport, &budget),
            Err(Failure::Timeout)
        );
        assert_eq!(transport.requests.lock().unwrap().len(), before);
    }

    #[test]
    fn malformed_conflicting_or_unpublished_dates_are_omitted() {
        let cancelled = AtomicBool::new(false);
        let transport = MockTransport::default();
        for body in [
            "Changes with nginx 1.31.60 15 Sep 2026",
            "Changes with nginx 1.31.6 31 Feb 2026",
            "Changes with nginx 1.31.6 15 Nope 2026",
            "Changes with nginx 1.31.6 15 Sep 2026\nChanges with nginx 1.31.6 16 Sep 2026",
        ] {
            transport.push("text/plain", body);
            assert!(
                ReleaseClient::default()
                    .lookup("nginx/1.31.6", &transport, &Budget::new(&cancelled))
                    .unwrap()
                    .is_none(),
                "{body}"
            );
        }
        transport.responses.lock().unwrap().push_back(HttpResponse {
            status: 404,
            ..HttpResponse::default()
        });
        assert!(
            ReleaseClient::default()
                .lookup("Caddy/2.0.0", &transport, &Budget::new(&cancelled))
                .unwrap()
                .is_none()
        );
    }
}
