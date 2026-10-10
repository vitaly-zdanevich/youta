//! Token-free registry WHOIS fallback with IANA discovery and bounded public I/O.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use html5gum::{DefaultEmitter, Token, Tokenizer};
use ureq::unversioned::resolver::Resolver;
use ureq::unversioned::transport::NextTimeout;
use url::{Host, Url};

use super::{
    Budget, DocumentKind, Facts, Failure, HttpTransport, PublicResolver, check_status,
    fetch_document, valid_domain, validate_public_url,
};

const MAX_WHOIS_BYTES: usize = 64 * 1024;
const MAX_CACHED_TLDS: usize = 64;

/// Session-only mappings discovered from IANA, never from untrusted website content.
#[derive(Default)]
pub(super) struct WhoisClient {
    servers: BTreeMap<String, String>,
}

impl WhoisClient {
    /// Retrieves only the matching registry domain record; referrals are not followed.
    pub(super) fn lookup(
        &mut self,
        host: &str,
        transport: &impl HttpTransport,
        budget: &Budget<'_>,
    ) -> Result<Vec<String>, Failure> {
        budget.remaining()?;
        if !valid_domain(host) || !host.contains('.') {
            return Err(Failure::Unavailable);
        }
        let tld = host.rsplit('.').next().ok_or(Failure::Unavailable)?;
        let server = if let Some(server) = self.servers.get(tld) {
            server.clone()
        } else {
            let mut url =
                Url::parse("https://www.iana.org/whois").map_err(|_| Failure::InvalidDocument)?;
            url.query_pairs_mut().append_pair("q", tld);
            let response = fetch_document(transport, &url, DocumentKind::Html, budget)?;
            check_status(response.response.status)?;
            let server = iana_server(&response.response.body, tld)?;
            if self.servers.len() >= MAX_CACHED_TLDS {
                self.servers.pop_first();
            }
            self.servers.insert(tld.to_owned(), server.clone());
            server
        };
        let mut candidate = host;
        for _ in 0..super::MAX_DOMAIN_ATTEMPTS {
            if !candidate.contains('.') {
                break;
            }
            let response =
                transport.whois(&server, candidate, budget.request()?, budget.cancelled)?;
            budget.remaining()?;
            match registry_facts(&response, candidate) {
                Ok(values) => {
                    let mut facts = Facts::default();
                    if candidate != host {
                        facts.add("Registered domain", candidate);
                    }
                    facts.extend(values);
                    facts.add("WHOIS source", &server);
                    return Ok(facts.finish());
                }
                Err(Failure::NotFound) => {
                    candidate = candidate.split_once('.').map_or("", |(_, parent)| parent);
                }
                Err(error) => return Err(error),
            }
        }
        Err(Failure::NotFound)
    }
}

/// Sends one CRLF-terminated domain query to a public registry on TCP port 43.
///
/// The resolver's finite timeout also covers DNS. Connections use its vetted
/// addresses directly, and every blocking operation consumes the same deadline.
/// WHOIS itself is unencrypted; only IANA endpoint discovery uses HTTPS.
pub(super) fn query(
    server: &str,
    domain: &str,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, Failure> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(Failure::Timeout)?;
    remaining(deadline, cancelled)?;
    let server = public_server(server)?;
    if !valid_domain(domain) || !domain.contains('.') {
        return Err(Failure::InvalidDocument);
    }
    // The HTTP-shaped URI is only an input to the existing public DNS resolver;
    // no HTTP request is made to the registry's WHOIS port.
    let uri = format!("http://{server}:43/")
        .parse()
        .map_err(|_| Failure::InvalidDocument)?;
    let addresses = PublicResolver::default()
        .resolve(
            &uri,
            &ureq::config::Config::default(),
            NextTimeout {
                after: remaining(deadline, cancelled)?.into(),
                reason: ureq::Timeout::Global,
            },
        )
        .map_err(|error| match error {
            ureq::Error::Timeout(_) => Failure::Timeout,
            _ => Failure::Transport,
        })?;
    let mut last_error = Failure::Transport;
    for (index, address) in addresses.iter().enumerate() {
        let attempts = (addresses.len() - index) as u32;
        let allowance = remaining(deadline, cancelled)? / attempts;
        if allowance.is_zero() {
            return Err(Failure::Timeout);
        }
        match TcpStream::connect_timeout(address, allowance) {
            Ok(stream) => return exchange(stream, domain, deadline, cancelled),
            Err(error) => last_error = io_failure(&error),
        }
    }
    remaining(deadline, cancelled)?;
    Err(last_error)
}

/// Checks the operation deadline without logging the untrusted query or response.
fn remaining(deadline: Instant, cancelled: &AtomicBool) -> Result<Duration, Failure> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(Failure::Cancelled);
    }
    deadline
        .checked_duration_since(Instant::now())
        .filter(|value| !value.is_zero())
        .ok_or(Failure::Timeout)
}

/// Reads until the registry closes the connection, never accepting a truncated response.
fn exchange(
    mut stream: TcpStream,
    domain: &str,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, Failure> {
    let request = format!("{domain}\r\n");
    let mut written = 0;
    while written < request.len() {
        stream
            .set_write_timeout(Some(remaining(deadline, cancelled)?))
            .map_err(|error| io_failure(&error))?;
        match stream.write(&request.as_bytes()[written..]) {
            Ok(0) => return Err(Failure::Transport),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_failure(&error)),
        }
    }
    let mut body = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        stream
            .set_read_timeout(Some(remaining(deadline, cancelled)?))
            .map_err(|error| io_failure(&error))?;
        let length = (MAX_WHOIS_BYTES + 1 - body.len()).min(chunk.len());
        match stream.read(&mut chunk[..length]) {
            Ok(0) => {
                remaining(deadline, cancelled)?;
                return Ok(body);
            }
            Ok(count) => {
                if body.len() + count > MAX_WHOIS_BYTES {
                    return Err(Failure::TooLarge);
                }
                body.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_failure(&error)),
        }
    }
}

/// Converts platform-specific socket timeout errors into the shared safe explanation.
fn io_failure(error: &std::io::Error) -> Failure {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Failure::Timeout,
        _ => Failure::Transport,
    }
}

/// Rejects address literals, private names, URLs, ports and query-injection characters.
fn public_server(value: &str) -> Result<String, Failure> {
    let server = value.trim_end_matches('.').to_ascii_lowercase();
    if !valid_domain(&server) || !server.contains('.') {
        return Err(Failure::InvalidDocument);
    }
    let url = Url::parse(&format!("https://{server}/")).map_err(|_| Failure::InvalidDocument)?;
    if !matches!(url.host(), Some(Host::Domain(_))) || validate_public_url(&url).is_err() {
        return Err(Failure::InvalidDocument);
    }
    Ok(server)
}

/// Accepts the WHOIS server from a completed, identity-matching IANA preformatted record.
fn iana_server(html: &[u8], tld: &str) -> Result<String, Failure> {
    if html.len() > super::MAX_HTML_BYTES {
        return Err(Failure::TooLarge);
    }
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut pre = None::<Vec<u8>>;
    let mut hidden_depth = 0_usize;
    for (index, token) in Tokenizer::new_with_emitter(html, emitter).enumerate() {
        if index >= super::MAX_HTML_TOKENS {
            return Err(Failure::TooLarge);
        }
        let Ok(token) = token;
        match token {
            Token::StartTag(tag) => {
                if matches!(
                    tag.name.as_slice(),
                    b"script" | b"style" | b"template" | b"noscript"
                ) {
                    hidden_depth = hidden_depth.saturating_add(1);
                } else if hidden_depth == 0 && tag.name.as_slice() == b"pre" {
                    if pre.is_some() {
                        return Err(Failure::InvalidDocument);
                    }
                    pre = Some(Vec::new());
                }
            }
            Token::EndTag(tag) => {
                if matches!(
                    tag.name.as_slice(),
                    b"script" | b"style" | b"template" | b"noscript"
                ) {
                    hidden_depth = hidden_depth.saturating_sub(1);
                } else if hidden_depth == 0 && tag.name.as_slice() == b"pre" {
                    let text = pre.take().ok_or(Failure::InvalidDocument)?;
                    return discovered_server(&text, tld);
                }
            }
            Token::String(text) if hidden_depth == 0 => {
                if let Some(pre) = &mut pre {
                    if pre.len().saturating_add(text.value.len()) > MAX_WHOIS_BYTES {
                        return Err(Failure::TooLarge);
                    }
                    pre.extend_from_slice(text.value.as_ref());
                }
            }
            _ => {}
        }
    }
    Err(Failure::InvalidDocument)
}

/// Only the delegation's WHOIS hostname is consumed; TLD dates are never domain facts.
fn discovered_server(text: &[u8], tld: &str) -> Result<String, Failure> {
    let text = std::str::from_utf8(text).map_err(|_| Failure::InvalidDocument)?;
    let mut matched_domain = false;
    let mut server = None;
    for (key, value) in text.lines().filter_map(field) {
        match key.as_str() {
            "domain" => {
                if matched_domain || !value.eq_ignore_ascii_case(tld) {
                    return Err(Failure::Mismatch);
                }
                matched_domain = true;
            }
            "whois" => {
                if server.is_some() {
                    return Err(Failure::InvalidDocument);
                }
                server = Some(public_server(value)?);
            }
            _ => {}
        }
    }
    if !matched_domain {
        return Err(Failure::Mismatch);
    }
    server.ok_or(Failure::Unavailable)
}

/// Splits a WHOIS field without treating comments, notices or whole URLs as fields.
fn field(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    if line.starts_with(['%', '#', '>']) {
        return None;
    }
    let (key, value) = line.split_once(':')?;
    Some((key.trim().to_ascii_lowercase(), value.trim()))
}

/// Extracts a small field allowlist only after confirming the exact queried domain.
fn registry_facts(body: &[u8], domain: &str) -> Result<Vec<String>, Failure> {
    if body.len() > MAX_WHOIS_BYTES {
        return Err(Failure::TooLarge);
    }
    let text = String::from_utf8_lossy(body);
    let lines = text.lines().collect::<Vec<_>>();
    let mut start = None;
    for (index, line) in lines.iter().enumerate() {
        if let Some((key, value)) = field(line)
            && matches!(key.as_str(), "domain" | "domain name")
        {
            if start.is_some() || !value.trim_end_matches('.').eq_ignore_ascii_case(domain) {
                return Err(Failure::Mismatch);
            }
            start = Some(index + 1);
        }
    }
    let Some(start) = start else {
        if lines.iter().any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("rate limit")
                || lower.contains("limit exceeded")
                || lower.contains("too many queries")
        }) {
            return Err(Failure::RateLimited);
        }
        return Err(
            if lines.iter().any(|line| explicit_no_match(line, domain)) {
                Failure::NotFound
            } else {
                Failure::InvalidDocument
            },
        );
    };
    let mut values = BTreeMap::new();
    let mut nameservers = Vec::new();
    let mut statuses = Vec::new();
    // A blank line ends the domain object. Contact objects can also have creation
    // dates; consuming the entire response would misattribute those to the domain.
    for line in lines[start..]
        .iter()
        .take_while(|line| !line.trim().is_empty())
    {
        let Some((key, value)) = field(line) else {
            continue;
        };
        if matches!(
            key.as_str(),
            "contact" | "nic-hdl" | "nic-hdl-br" | "person" | "personname" | "role"
        ) {
            break;
        }
        let label = match key.as_str() {
            "created" | "creation date" | "created on" | "registered on" | "registration date"
            | "registration time" => Some("Registered"),
            "changed" | "updated date" | "last modified" | "last updated on" | "modified" => {
                Some("Changed")
            }
            "paid-till"
            | "expires"
            | "expiry date"
            | "expiration date"
            | "registry expiry date"
            | "registrar registration expiration date"
            | "expires on" => Some("Expires"),
            "registrar" | "registrar name" => Some("Registrar"),
            "nserver" | "name server" | "nameserver" if nameservers.len() < 16 => {
                if let Some(name) = value.split_whitespace().next() {
                    let name = name.trim_end_matches('.').to_ascii_lowercase();
                    if valid_domain(&name) && !nameservers.contains(&name) {
                        nameservers.push(name);
                    }
                }
                None
            }
            "state" | "status" | "domain status" if statuses.len() < 16 => {
                let value = super::safe_text(value);
                if !value.is_empty() && !statuses.contains(&value) {
                    statuses.push(value);
                }
                None
            }
            _ => None,
        };
        if let Some(label) = label {
            values.entry(label).or_insert(value);
        }
    }
    let mut facts = Facts::default();
    for label in ["Registered", "Changed", "Expires", "Registrar"] {
        if let Some(value) = values.get(label) {
            facts.add(label, value);
        }
    }
    facts.add("Nameservers", &nameservers.join(", "));
    facts.add("Domain status", &statuses.join(", "));
    Ok(facts.finish())
}

/// Only recognized negative replies authorize looking up a parent domain.
fn explicit_no_match(line: &str, domain: &str) -> bool {
    let line = line
        .trim()
        .trim_start_matches('%')
        .trim()
        .to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    matches!(
        line.as_str(),
        "no entries found for the selected source(s)."
            | "no entries found."
            | "not found"
            | "no data found"
    ) || line == format!("no match for \"{domain}\".")
        || line == format!("no match for domain \"{domain}\".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url_info::{DocumentKind, HttpResponse};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Instant;
    use url::Url;

    const IANA_RU: &str = "<html><body><pre>% IANA WHOIS server\n\
        domain: RU\ncreated: 1994-04-07\nwhois: whois.tcinet.ru\n</pre></body></html>";
    const PREDANIE: &str = "% .ru registry\n\n\
        domain: PREDANIE.RU\nnserver: ns1.example.net. 8.8.8.8\n\
        nserver: ns2.example.net.\nstate: REGISTERED, DELEGATED, VERIFIED\n\
        registrar: REGTIME-RU\ncreated: 2005-12-12T21:00:00Z\n\
        paid-till: 2026-12-12T21:00:00Z\nfree-date: 2027-01-13\n\
        source: TCI\n\n% End of response\n";

    /// Separate scripted transports make discovery and registry requests observable.
    #[derive(Default)]
    struct MockTransport {
        http: Mutex<VecDeque<(String, Vec<u8>)>>,
        whois: Mutex<VecDeque<(String, String, Result<Vec<u8>, Failure>)>>,
        requests: Mutex<Vec<String>>,
    }

    impl MockTransport {
        fn discovery(&self, tld: &str, html: &str) {
            self.http.lock().unwrap().push_back((
                format!("https://www.iana.org/whois?q={tld}"),
                html.as_bytes().to_vec(),
            ));
        }

        fn record(&self, domain: &str, body: &str) {
            self.whois.lock().unwrap().push_back((
                "whois.tcinet.ru".into(),
                domain.into(),
                Ok(body.as_bytes().to_vec()),
            ));
        }
    }

    impl HttpTransport for MockTransport {
        fn fetch(
            &self,
            url: &Url,
            _: DocumentKind,
            _: Duration,
            _: &AtomicBool,
        ) -> Result<HttpResponse, Failure> {
            self.requests.lock().unwrap().push(url.to_string());
            let (expected, body) = self.http.lock().unwrap().pop_front().unwrap();
            assert_eq!(url.as_str(), expected);
            Ok(HttpResponse {
                status: 200,
                location: None,
                content_type: "text/html".into(),
                body,
                ..HttpResponse::default()
            })
        }

        fn whois(
            &self,
            server: &str,
            domain: &str,
            _: Duration,
            _: &AtomicBool,
        ) -> Result<Vec<u8>, Failure> {
            self.requests
                .lock()
                .unwrap()
                .push(format!("{server}/{domain}"));
            let (expected_server, expected_domain, response) =
                self.whois.lock().unwrap().pop_front().unwrap();
            assert_eq!(server, expected_server);
            assert_eq!(domain, expected_domain);
            response
        }
    }

    #[test]
    fn predanie_creation_date_comes_from_registry_not_iana() {
        let transport = MockTransport::default();
        transport.discovery("ru", IANA_RU);
        transport.record("predanie.ru", PREDANIE);
        let facts = WhoisClient::default()
            .lookup(
                "predanie.ru",
                &transport,
                &Budget::new(&AtomicBool::new(false)),
            )
            .unwrap();
        assert!(facts.contains(&"Registered: 2005-12-12T21:00:00Z".into()));
        assert!(facts.contains(&"Expires: 2026-12-12T21:00:00Z".into()));
        assert!(facts.contains(&"Registrar: REGTIME-RU".into()));
        assert!(facts.contains(&"Nameservers: ns1.example.net, ns2.example.net".into()));
        assert!(facts.contains(&"WHOIS source: whois.tcinet.ru".into()));
        assert!(!facts.join("\n").contains("1994"));
        assert!(!facts.join("\n").contains("2027"));
    }

    #[test]
    fn discovery_requires_matching_completed_visible_record_and_public_server() {
        assert_eq!(
            iana_server(IANA_RU.as_bytes(), "ru").unwrap(),
            "whois.tcinet.ru"
        );
        for html in [
            "domain: RU\nwhois: whois.tcinet.ru",
            "<pre>domain: COM\nwhois: whois.tcinet.ru</pre>",
            "<pre>domain: RU\nwhois: whois.tcinet.ru",
            "<pre>domain: RU\nwhois: whois.tcinet.ru\ndomain: COM</pre>",
            "<script><pre>domain: RU\nwhois: whois.tcinet.ru</pre></script>",
            "<template><pre>domain: RU\nwhois: whois.tcinet.ru</pre></template>",
            "<pre>domain: RU\nwhois: whois.tcinet.ru\nwhois: evil.example</pre>",
        ] {
            assert!(iana_server(html.as_bytes(), "ru").is_err(), "{html}");
        }
        for server in [
            "localhost",
            "127.0.0.1",
            "10.0.0.1",
            "host.local",
            "host.internal",
            "whois.tcinet.ru:80",
            "user@whois.tcinet.ru",
            "https://whois.tcinet.ru/",
            "whois.tcinet.ru/secret",
        ] {
            assert!(
                iana_server(
                    format!("<pre>domain: RU\nwhois: {server}</pre>").as_bytes(),
                    "ru"
                )
                .is_err(),
                "{server}"
            );
        }
    }

    #[test]
    fn domain_identity_is_required_and_contacts_do_not_supply_dates() {
        for body in [
            "created: 2005-12-12T21:00:00Z",
            "domain: other.ru\ncreated: 2005-12-12T21:00:00Z",
            "domain: predanie.ru\ndomain: other.ru\ncreated: 2005-12-12T21:00:00Z",
        ] {
            assert!(
                registry_facts(body.as_bytes(), "predanie.ru").is_err(),
                "{body}"
            );
        }
        let facts = registry_facts(
            b"domain: predanie.ru\nregistrar: Registry\n\ncontact: Person\ncreated: 2020-01-01\n",
            "predanie.ru",
        )
        .unwrap();
        assert_eq!(facts, ["Registrar: Registry"]);
        assert_eq!(registry_facts(b"Domain Name: PREDANIE.RU\nCreation Date: 2005-12-12T21:00:00Z\nUpdated Date: 2025-01-01\nRegistry Expiry Date: 2026-12-12\n", "predanie.ru").unwrap(), ["Registered: 2005-12-12T21:00:00Z", "Changed: 2025-01-01", "Expires: 2026-12-12"]);
    }

    #[test]
    fn only_explicit_not_found_allows_bounded_parent_retry() {
        let transport = MockTransport::default();
        transport.discovery("ru", IANA_RU);
        transport.record(
            "www.predanie.ru",
            "% No entries found for the selected source(s).\n",
        );
        transport.record("predanie.ru", PREDANIE);
        let facts = WhoisClient::default()
            .lookup(
                "www.predanie.ru",
                &transport,
                &Budget::new(&AtomicBool::new(false)),
            )
            .unwrap();
        assert!(facts.contains(&"Registered domain: predanie.ru".into()));
        for record in [
            "temporarily unavailable",
            "domain: wrong.ru\ncreated: 2000-01-01",
            "% Query rate limit exceeded",
        ] {
            let transport = MockTransport::default();
            transport.discovery("ru", IANA_RU);
            transport.record("www.predanie.ru", record);
            assert!(
                WhoisClient::default()
                    .lookup(
                        "www.predanie.ru",
                        &transport,
                        &Budget::new(&AtomicBool::new(false))
                    )
                    .is_err()
            );
            assert_eq!(transport.requests.lock().unwrap().len(), 2);
        }
        let transport = MockTransport::default();
        transport.discovery("ru", IANA_RU);
        for domain in [
            "a.b.c.d.e.predanie.ru",
            "b.c.d.e.predanie.ru",
            "c.d.e.predanie.ru",
            "d.e.predanie.ru",
            "e.predanie.ru",
        ] {
            transport.record(domain, "% No entries found for the selected source(s).\n");
        }
        assert_eq!(
            WhoisClient::default().lookup(
                "a.b.c.d.e.predanie.ru",
                &transport,
                &Budget::new(&AtomicBool::new(false))
            ),
            Err(Failure::NotFound)
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 6);
    }

    #[test]
    fn successful_discovery_is_cached_and_cache_is_bounded() {
        let transport = MockTransport::default();
        transport.discovery("ru", IANA_RU);
        transport.record("predanie.ru", PREDANIE);
        transport.record("predanie.ru", PREDANIE);
        let mut client = WhoisClient::default();
        for _ in 0..2 {
            client
                .lookup(
                    "predanie.ru",
                    &transport,
                    &Budget::new(&AtomicBool::new(false)),
                )
                .unwrap();
        }
        assert_eq!(transport.requests.lock().unwrap().len(), 3);
        for index in 0..MAX_CACHED_TLDS + 1 {
            let tld = format!("t{index}");
            transport.discovery(
                &tld,
                &format!("<pre>domain: {tld}\nwhois: whois.tcinet.ru</pre>"),
            );
            let domain = format!("example.{tld}");
            transport.record(&domain, &format!("domain: {domain}\ncreated: 2020-01-01"));
            client
                .lookup(&domain, &transport, &Budget::new(&AtomicBool::new(false)))
                .unwrap();
        }
        assert!(client.servers.len() <= MAX_CACHED_TLDS);
    }

    #[test]
    fn response_limits_and_cancelled_expired_requests_are_enforced() {
        assert_eq!(
            registry_facts(&vec![b'x'; MAX_WHOIS_BYTES + 1], "predanie.ru"),
            Err(Failure::TooLarge)
        );
        assert_eq!(
            iana_server(&vec![b'x'; super::super::MAX_HTML_BYTES + 1], "ru"),
            Err(Failure::TooLarge)
        );
        assert_eq!(
            query(
                "whois.tcinet.ru",
                "predanie.ru",
                Duration::from_secs(1),
                &AtomicBool::new(true)
            ),
            Err(Failure::Cancelled)
        );
        assert_eq!(
            query(
                "whois.tcinet.ru",
                "predanie.ru",
                Duration::ZERO,
                &AtomicBool::new(false)
            ),
            Err(Failure::Timeout)
        );
        let transport = MockTransport::default();
        assert_eq!(
            WhoisClient::default().lookup(
                "predanie.ru",
                &transport,
                &Budget::new(&AtomicBool::new(true))
            ),
            Err(Failure::Cancelled)
        );
        let cancelled = AtomicBool::new(false);
        let mut budget = Budget::new(&cancelled);
        budget.deadline = Instant::now();
        assert_eq!(
            WhoisClient::default().lookup("predanie.ru", &transport, &budget),
            Err(Failure::Timeout)
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    /// Registry replies cannot redirect the fallback or authorize unrelated parent queries.
    #[test]
    fn referrals_and_mismatched_negative_replies_are_not_followed() {
        let transport = MockTransport::default();
        transport.discovery("ru", IANA_RU);
        transport.record(
            "predanie.ru",
            "domain: predanie.ru\ncreated: 2005-12-12\nWhois Server: localhost:25\n",
        );
        let facts = WhoisClient::default()
            .lookup(
                "predanie.ru",
                &transport,
                &Budget::new(&AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(
            facts,
            ["Registered: 2005-12-12", "WHOIS source: whois.tcinet.ru"]
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 2);
        assert_eq!(
            registry_facts(b"No match for \"OTHER.RU\".\n", "predanie.ru"),
            Err(Failure::InvalidDocument)
        );
        for server in [
            "localhost",
            "127.0.0.1",
            "whois.tcinet.ru:80",
            "whois.tcinet.ru\r\n",
        ] {
            assert_eq!(
                query(
                    server,
                    "predanie.ru",
                    Duration::from_secs(1),
                    &AtomicBool::new(false)
                ),
                Err(Failure::InvalidDocument)
            );
        }
        assert_eq!(
            query(
                "whois.tcinet.ru",
                "predanie.ru\r\nother.ru",
                Duration::from_secs(1),
                &AtomicBool::new(false)
            ),
            Err(Failure::InvalidDocument)
        );
    }

    /// A mock registry checks the wire format and completion, without public network requests.
    #[test]
    fn socket_exchange_sends_one_crlf_query_and_requires_complete_bounded_reply() {
        use std::io::BufRead;
        use std::net::TcpListener;

        for body in [
            PREDANIE.as_bytes().to_vec(),
            vec![b'x'; MAX_WHOIS_BYTES + 1],
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut server, _) = listener.accept().unwrap();
            let expected = body.clone();
            std::thread::scope(|scope| {
                let registry = scope.spawn(move || {
                    server
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = String::new();
                    std::io::BufReader::new(&server)
                        .read_line(&mut request)
                        .unwrap();
                    assert_eq!(request, "predanie.ru\r\n");
                    let _ = server.write_all(&body);
                });
                let response = exchange(
                    client,
                    "predanie.ru",
                    Instant::now() + Duration::from_secs(2),
                    &AtomicBool::new(false),
                );
                if expected.len() > MAX_WHOIS_BYTES {
                    assert_eq!(response, Err(Failure::TooLarge));
                } else {
                    assert_eq!(response.unwrap(), expected);
                }
                registry.join().unwrap();
            });
        }
    }

    /// Keeping a socket open cannot evade the absolute deadline or yield a partial record.
    #[test]
    fn socket_exchange_times_out_without_accepting_partial_reply() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.write_all(PREDANIE.as_bytes()).unwrap();
        assert_eq!(
            exchange(
                client,
                "predanie.ru",
                Instant::now() + Duration::from_millis(50),
                &AtomicBool::new(false)
            ),
            Err(Failure::Timeout)
        );
    }

    /// Opt-in end-to-end check of the domain that exposed the missing RDAP fallback.
    #[test]
    #[ignore = "requires public HTTPS and registry WHOIS access"]
    fn live_predanie_lookup_returns_domain_creation_date() {
        let facts = crate::url_info::UrlInfoClient::default().lookup(
            &Url::parse("https://predanie.ru/").unwrap(),
            &AtomicBool::new(false),
        );
        assert!(
            facts.contains(&"Registered: 2005-12-12T21:00:00Z".into()),
            "{facts:?}"
        );
        assert!(
            !facts
                .iter()
                .any(|fact| fact.starts_with("Registered: 1994"))
        );
    }
}
