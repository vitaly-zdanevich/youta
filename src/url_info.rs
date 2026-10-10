//! Bounded, public-network website metadata and domain-registration facts.
//!
//! Only an explicit controller request reaches this client. Website declarations
//! are unverified claims; registry dates are not website age or a trust rating.
//! Explicit website requests preserve their query strings without sharing them
//! with registration services. No cookies, URL credentials, browser state,
//! scripts, or related resources are loaded. Registration entities are parsed
//! locally, never crawled. Website metadata uses at most the first 256 KiB;
//! registration JSON must fit completely within its separate response limit.

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Connector, NextTimeout, TcpConnector};
use url::{Host, Url};

use crate::domain::{ip_address_is_non_public, remote_url_has_non_public_host};

mod analytics;
mod metadata;
mod network;
mod server_release;
pub(crate) mod site_files;
mod tls;
mod whois;

const BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/dns.json";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_HTML_BYTES: usize = 256 * 1024;
const MAX_JSON_BYTES: usize = 512 * 1024;
const MAX_JSON_DEPTH: usize = 24;
const MAX_JSON_NODES: usize = 32_768;
const MAX_HTML_TOKENS: usize = 16_384;
const MAX_URL_BYTES: usize = 4_096;
const MAX_FIELD_BYTES: usize = 1_024;
const MAX_FACTS: usize = 48;
const MAX_FACT_BYTES: usize = 16 * 1024;
const MAX_REDIRECTS: usize = 3;
const MAX_REQUESTS: usize = 24;
const MAX_DOMAIN_ATTEMPTS: usize = 5;
const MAX_BOOTSTRAP_SUFFIXES: usize = 4_096;

/// One session's cached IANA endpoint map; neither response bodies nor page claims persist.
#[derive(Default)]
pub(crate) struct UrlInfoClient {
    bootstrap: Option<Vec<BootstrapService>>,
    whois: whois::WhoisClient,
    network: network::NetworkClient,
    releases: server_release::ReleaseClient,
}

impl UrlInfoClient {
    /// Returns bounded website and registration facts, retaining either partial result.
    ///
    /// The caller must use a worker. One shared eight-second budget covers DNS,
    /// concurrent website/registration requests, redirects, and body reads.
    /// Cancellation is checked between reads; both branches finish before return.
    /// Errors are fixed explanations and never echo credentials or request queries.
    /// The final URL is shown only when it differs from the fragment-free original.
    pub(crate) fn lookup(&mut self, url: &Url, cancelled: &AtomicBool) -> Vec<String> {
        self.lookup_with(url, cancelled, &UreqTransport::default())
    }

    /// Keeps all URL and redirect policy in the same path for real and mocked transports.
    fn lookup_with(
        &mut self,
        url: &Url,
        cancelled: &AtomicBool,
        transport: &impl HttpTransport,
    ) -> Vec<String> {
        let mut facts = Facts::default();
        if let Err(error) = validate_public_url(url) {
            facts.add("URL info", &error.message());
            return facts.finish();
        }
        let budget = Budget::new(cancelled);
        // The website worker owns the IP bootstrap cache. A mutex also permits
        // the sequential fallback without retaining a mutable scoped borrow.
        let caches = Mutex::new((
            std::mem::take(&mut self.network),
            std::mem::take(&mut self.releases),
        ));
        let website = || {
            let mut caches = caches.lock().map_err(|_| Failure::Transport)?;
            let (network, releases) = &mut *caches;
            fetch_website(transport, url, &budget, network, releases)
        };
        let (page, registration) = if let Some(Host::Domain(host)) = url.host() {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            std::thread::scope(|scope| {
                // Registration keeps sole ownership of the cached endpoint map.
                // The website shares the deadline and atomic request allowance.
                match std::thread::Builder::new()
                    .name("youta-url-website".into())
                    .spawn_scoped(scope, website)
                {
                    Ok(website) => {
                        let registration = self.registration(&host, transport, &budget);
                        let page = website.join().unwrap_or(Err(Failure::Transport));
                        (page, Some(registration))
                    }
                    Err(_) => {
                        // Thread limits must not make otherwise usable lookups fail.
                        let page = website();
                        (page, Some(self.registration(&host, transport, &budget)))
                    }
                }
            })
        } else {
            (website(), None)
        };
        (self.network, self.releases) = caches.into_inner().unwrap_or_default();
        // Keep display order deterministic even when registration finishes first.
        match page {
            Ok((page, network_facts)) => {
                facts.add(
                    "Website response",
                    &format!("HTTP {}", page.response.status),
                );
                // Fragments are not sent to the server, so their removal is
                // not a changed destination worth repeating in the facts.
                let mut requested_url = url.clone();
                requested_url.set_fragment(None);
                if page.url != requested_url {
                    facts.add("Final URL", page.url.as_str());
                }
                if !page.redirects.is_empty() {
                    let chain = page
                        .redirects
                        .iter()
                        .chain(std::iter::once(&page.url))
                        .map(Url::as_str)
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    facts.add("Redirect chain", &chain);
                }
                if let Some(ip) = page.response.peer_ip {
                    facts.add("IP address", &ip.to_string());
                }
                facts.add("Server", &page.response.server);
                facts.add("Compression", &page.response.compression);
                if let Some(bytes) = page.response.content_length {
                    facts.add("Content length", &human_content_length(bytes));
                }
                facts.add("CDN (reported)", &page.response.reported_cdns.join(", "));
                if let Some(tls) = page.response.tls_info {
                    facts.extend(tls.facts);
                }
                facts.extend(network_facts);
                if (200..300).contains(&page.response.status) {
                    match metadata::facts(&page.response.body, Some(&page.url)) {
                        Ok(values) => facts.extend(values),
                        Err(error) => facts.add("Website metadata", &error.message()),
                    }
                    facts.add(
                        "Analytics in HTML",
                        &analytics::detect(&page.response.body).join(", "),
                    );
                }
            }
            Err(error) => facts.add("Website", &error.message()),
        }
        if cancelled.load(Ordering::Relaxed) {
            return facts.finish();
        }
        let Some(registration) = registration else {
            facts.add(
                "Registration",
                "Domain registration is not applicable to an IP address",
            );
            return facts.finish();
        };
        match registration {
            Ok(values) => facts.extend(values),
            Err(error) => facts.add("Registration", &error.message()),
        }
        facts.finish()
    }

    /// Uses WHOIS only when IANA publishes no supported domain RDAP service.
    fn registration(
        &mut self,
        host: &str,
        transport: &impl HttpTransport,
        budget: &Budget<'_>,
    ) -> Result<Vec<String>, Failure> {
        match self.rdap_registration(host, transport, budget) {
            Err(Failure::Unavailable) => self.whois.lookup(host, transport, budget),
            result => result,
        }
    }

    /// Tries a parent only after a registry 404, never guessing a registrable suffix.
    ///
    /// Only a validated parent needs a domain label: the original URL already
    /// identifies an exact hostname match, even when its website redirects elsewhere.
    fn rdap_registration(
        &mut self,
        host: &str,
        transport: &impl HttpTransport,
        budget: &Budget<'_>,
    ) -> Result<Vec<String>, Failure> {
        if !valid_domain(host) {
            return Err(Failure::Unavailable);
        }
        if self.bootstrap.is_none() {
            let source = Url::parse(BOOTSTRAP_URL).map_err(|_| Failure::InvalidDocument)?;
            let response = fetch_document(transport, &source, DocumentKind::Json, budget)?;
            check_status(response.response.status)?;
            self.bootstrap = Some(parse_bootstrap(&parse_json(&response.response.body)?)?);
        }
        let service = self
            .bootstrap
            .as_ref()
            .and_then(|services| {
                services
                    .iter()
                    .filter(|service| {
                        host == service.suffix
                            || host
                                .strip_suffix(&service.suffix)
                                .is_some_and(|prefix| prefix.ends_with('.'))
                    })
                    .max_by_key(|service| service.suffix.len())
            })
            .ok_or(Failure::Unavailable)?;
        let mut candidate = host;
        for _ in 0..MAX_DOMAIN_ATTEMPTS {
            if !candidate.contains('.') {
                break;
            }
            let mut source = service.endpoint.clone();
            {
                let mut path = source
                    .path_segments_mut()
                    .map_err(|_| Failure::InvalidDocument)?;
                path.pop_if_empty().push("domain").push(candidate);
            }
            let response = fetch_document(transport, &source, DocumentKind::Json, budget)?;
            if response.response.status == 404 {
                candidate = candidate.split_once('.').map_or("", |(_, parent)| parent);
                continue;
            }
            check_status(response.response.status)?;
            let values = rdap_facts(&parse_json(&response.response.body)?, candidate)?;
            let mut facts = Facts::default();
            if candidate != host {
                facts.add("Registered domain", candidate);
            }
            facts.extend(values);
            facts.add("RDAP source", response.url.as_str());
            return Ok(facts.finish());
        }
        Err(Failure::NotFound)
    }
}

/// Enriches the final website response using its connected peer, preserving the page on failure.
fn fetch_website(
    transport: &impl HttpTransport,
    url: &Url,
    budget: &Budget<'_>,
    network: &mut network::NetworkClient,
    releases: &mut server_release::ReleaseClient,
) -> Result<(FetchedDocument, Vec<String>), Failure> {
    let page = fetch_document(transport, url, DocumentKind::Html, budget)?;
    let releases = Mutex::new(releases);
    let release_lookup = || {
        releases
            .lock()
            .ok()?
            .lookup(&page.response.server, transport, budget)
            .ok()
            .flatten()
    };
    let mut network_lookup = || {
        page.response
            .peer_ip
            .filter(|ip| !ip_address_is_non_public(*ip))
            .and_then(|ip| network.lookup(ip, transport, budget).ok())
            .unwrap_or_default()
    };
    // The IP registry and software release source are independent once the page arrives.
    let (network, release) = std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .name("youta-server-release".into())
            .spawn_scoped(scope, release_lookup)
        {
            Ok(worker) => (network_lookup(), worker.join().unwrap_or_default()),
            Err(_) => (network_lookup(), release_lookup()),
        }
    });
    let mut facts = Facts::default();
    facts.extend(network);
    if let Some(release) = release {
        facts.add(
            "Server version released (upstream)",
            &format!(
                "{} {} - {}",
                release.product, release.version, release.released
            ),
        );
        facts.add("Server release source", &release.source);
    }
    Ok((page, facts.finish()))
}

/// Fixed errors omit request URLs and third-party response/error bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    InvalidUrl,
    Transport,
    Timeout,
    Cancelled,
    Budget,
    TooLarge,
    InvalidDocument,
    Redirect,
    WrongType,
    NotFound,
    RateLimited,
    Status(u16),
    Mismatch,
    Unavailable,
}

impl Failure {
    fn message(self) -> String {
        match self {
			Self::InvalidUrl => "Only public HTTP(S) URLs on ports 80/443 without URL credentials are supported".to_owned(),
			Self::Transport => "Could not connect to the public server".to_owned(),
			Self::Timeout => "The URL information time limit was reached".to_owned(),
			Self::Cancelled => "Lookup cancelled".to_owned(),
			Self::Budget => "The request limit was reached".to_owned(),
			Self::TooLarge => "The response exceeded the size limit".to_owned(),
			Self::InvalidDocument => "The server returned unsupported or malformed metadata".to_owned(),
			Self::Redirect => "The redirect was unsafe or exceeded the redirect limit".to_owned(),
			Self::WrongType => "The response was not the expected HTML or JSON document".to_owned(),
			Self::NotFound => "No matching registration record was published for this hostname or its queried parents".to_owned(),
			Self::RateLimited => "The registration service rate-limited this lookup; try later".to_owned(),
			Self::Status(status) => format!("The server returned HTTP {status}"),
			Self::Mismatch => "The registration response did not identify the requested domain".to_owned(),
			Self::Unavailable => "No supported registration service is published for this domain".to_owned(),
		}
    }
}

/// One deadline and atomic request allowance shared by concurrent lookup branches.
struct Budget<'a> {
    deadline: Instant,
    remaining_requests: AtomicUsize,
    cancelled: &'a AtomicBool,
}

impl<'a> Budget<'a> {
    fn new(cancelled: &'a AtomicBool) -> Self {
        Self {
            deadline: Instant::now() + LOOKUP_TIMEOUT,
            remaining_requests: AtomicUsize::new(MAX_REQUESTS),
            cancelled,
        }
    }

    fn remaining(&self) -> Result<Duration, Failure> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(Failure::Timeout)
    }

    fn request(&self) -> Result<Duration, Failure> {
        let remaining = self.remaining()?;
        self.remaining_requests
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .map_err(|_| Failure::Budget)?;
        Ok(remaining)
    }
}

/// Different body limits and MIME checks apply to website and registration documents.
#[derive(Clone, Copy)]
enum DocumentKind {
    Html,
    Json,
    Text,
}

impl DocumentKind {
    fn limit(self) -> usize {
        match self {
            Self::Html => MAX_HTML_BYTES,
            Self::Json => MAX_JSON_BYTES,
            Self::Text => site_files::MAX_FILE_BYTES,
        }
    }
    fn accepts(self, content_type: &str) -> bool {
        let content_type = content_type.split(';').next().unwrap_or("").trim();
        match self {
            Self::Html => {
                content_type.eq_ignore_ascii_case("text/html")
                    || content_type.eq_ignore_ascii_case("application/xhtml+xml")
            }
            Self::Json => {
                content_type.eq_ignore_ascii_case("application/json")
                    || content_type.eq_ignore_ascii_case("application/rdap+json")
            }
            Self::Text => [
                "text/plain",
                "text/xml",
                "application/xml",
                "application/gzip",
                "application/x-gzip",
                "application/octet-stream",
            ]
            .iter()
            .any(|expected| content_type.eq_ignore_ascii_case(expected)),
        }
    }
}

/// One bounded HTTP response; redirect and error bodies are deliberately not retained.
#[derive(Debug, Default)]
struct HttpResponse {
    status: u16,
    location: Option<String>,
    content_type: String,
    body: Vec<u8>,
    peer_ip: Option<std::net::IpAddr>,
    reported_cdns: Vec<&'static str>,
    tls_info: Option<tls::TlsInfo>,
    server: String,
    compression: String,
    content_length: Option<u64>,
}

#[derive(Debug)]
struct FetchedDocument {
    url: Url,
    response: HttpResponse,
    redirects: Vec<Url>,
}

/// Injectable I/O boundary; callers independently enforce redirects and document budgets.
trait HttpTransport: Sync {
    fn fetch(
        &self,
        url: &Url,
        kind: DocumentKind,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<HttpResponse, Failure>;

    /// Queries only the domain through the registry's IANA-discovered WHOIS server.
    fn whois(
        &self,
        _server: &str,
        _domain: &str,
        _timeout: Duration,
        _cancelled: &AtomicBool,
    ) -> Result<Vec<u8>, Failure> {
        Err(Failure::Unavailable)
    }
}

/// Validates every destination before the transport can perform DNS or HTTP I/O.
fn validate_public_url(url: &Url) -> Result<(), Failure> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.port_or_known_default(), Some(80 | 443))
        || url.as_str().len() > MAX_URL_BYTES
        || remote_url_has_non_public_host(url)
        || url.as_str().chars().any(char::is_control)
    {
        return Err(Failure::InvalidUrl);
    }
    Ok(())
}

/// Redirects share the operation deadline and may never downgrade an encrypted request.
fn fetch_document(
    transport: &impl HttpTransport,
    url: &Url,
    kind: DocumentKind,
    budget: &Budget<'_>,
) -> Result<FetchedDocument, Failure> {
    let mut current = url.clone();
    current.set_fragment(None);
    let mut chain = Vec::new();
    for redirects in 0..=MAX_REDIRECTS {
        validate_public_url(&current)?;
        if matches!(kind, DocumentKind::Json) && current.scheme() != "https" {
            return Err(Failure::InvalidUrl);
        }
        let response = transport.fetch(&current, kind, budget.request()?, budget.cancelled)?;
        budget.remaining()?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            if redirects == MAX_REDIRECTS {
                return Err(Failure::Redirect);
            }
            let location = response
                .location
                .as_deref()
                .filter(|value| value.len() <= MAX_URL_BYTES)
                .ok_or(Failure::Redirect)?;
            let mut next = current.join(location).map_err(|_| Failure::Redirect)?;
            validate_public_url(&next).map_err(|_| Failure::Redirect)?;
            if current.scheme() == "https" && next.scheme() != "https" {
                return Err(Failure::Redirect);
            }
            next.set_fragment(None);
            chain.push(current);
            current = next;
            continue;
        }
        if response.body.len() > kind.limit() {
            return Err(Failure::TooLarge);
        }
        if (200..300).contains(&response.status) && !kind.accepts(&response.content_type) {
            return Err(Failure::WrongType);
        }
        return Ok(FetchedDocument {
            url: current,
            response,
            redirects: chain,
        });
    }
    Err(Failure::Redirect)
}

/// Each hop gets a fresh credential-free agent, so cookies cannot cross requests.
#[derive(Default)]
struct UreqTransport {
    #[cfg(test)]
    allow_loopback: bool,
}

impl HttpTransport for UreqTransport {
    fn whois(
        &self,
        server: &str,
        domain: &str,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Vec<u8>, Failure> {
        whois::query(server, domain, timeout, cancelled)
    }

    fn fetch(
        &self,
        url: &Url,
        kind: DocumentKind,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<HttpResponse, Failure> {
        #[cfg(test)]
        let test_loopback = self.allow_loopback
            && matches!(url.host(), Some(Host::Ipv4(ip)) if ip.is_loopback())
            && url.scheme() == "http"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none();
        #[cfg(not(test))]
        let test_loopback = false;
        if !test_loopback {
            validate_public_url(url)?;
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        if timeout.is_zero() {
            return Err(Failure::Timeout);
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .timeout_resolve(Some(timeout))
            .timeout_connect(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .proxy(None)
            .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
            .build();
        let resolver = PublicResolver {
            inner: DefaultResolver::default(),
            #[cfg(test)]
            allow_loopback: test_loopback,
        };
        let peer = Arc::new(Mutex::new(None));
        let tls = Arc::new(Mutex::new(None));
        let connector = network::PeerConnector::with_connector(
            Arc::clone(&peer),
            TcpConnector::default().chain(tls::InfoTlsConnector::new(Arc::clone(&tls))),
        );
        #[cfg(test)]
        let connector = {
            let mut connector = connector;
            connector.allow_loopback = test_loopback;
            connector
        };
        let agent = ureq::Agent::with_parts(config, connector, resolver);
        let mut response = agent
            .get(url.as_str())
            .header(
                "Accept",
                match kind {
                    DocumentKind::Html => "text/html,application/xhtml+xml",
                    DocumentKind::Json => "application/rdap+json,application/json",
                    DocumentKind::Text => "text/plain,application/xml,text/xml,application/gzip,application/octet-stream",
                },
            )
            .call()
            .map_err(|error| match error {
                ureq::Error::Timeout(_) => Failure::Timeout,
                _ => Failure::Transport,
            })?;
        let status = response.status().as_u16();
        let content_length = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok());
        let server = response
            .headers()
            .get("server")
            .and_then(|value| value.to_str().ok())
            .map(safe_text)
            .unwrap_or_default();
        let compression = response
            .headers()
            .get_all("content-encoding")
            .iter()
            .take(8)
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|value| !value.eq_ignore_ascii_case("identity"))
            .map(safe_text)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let reported_cdns = network::reported_cdns(response.headers());
        let location = response
            .headers()
            .get("location")
            .map(|value| value.to_str().map(str::to_owned))
            .transpose()
            .map_err(|_| Failure::Redirect)?;
        if location
            .as_ref()
            .is_some_and(|value| value.len() > MAX_URL_BYTES)
        {
            return Err(Failure::Redirect);
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let body = if (200..300).contains(&status) {
            if !kind.accepts(&content_type) {
                return Err(Failure::WrongType);
            }
            if !matches!(kind, DocumentKind::Html)
                && response
                    .body()
                    .content_length()
                    .is_some_and(|length| length > kind.limit() as u64)
            {
                return Err(Failure::TooLarge);
            }
            let reader = response
                .body_mut()
                .with_config()
                .limit((kind.limit() + 1) as u64)
                .reader();
            match kind {
                // A large page can still publish useful metadata in its head. Stop
                // at the decoded-byte prefix without waiting for the remaining body.
                DocumentKind::Html => read_body(
                    reader.take(MAX_HTML_BYTES as u64),
                    MAX_HTML_BYTES,
                    cancelled,
                )?,
                DocumentKind::Json => read_body(reader, MAX_JSON_BYTES, cancelled)?,
                DocumentKind::Text => read_body(reader, site_files::MAX_FILE_BYTES, cancelled)?,
            }
        } else {
            Vec::new()
        };
        Ok(HttpResponse {
            status,
            location,
            content_type,
            body,
            peer_ip: *peer.lock().map_err(|_| Failure::Transport)?,
            tls_info: tls.lock().map_err(|_| Failure::Transport)?.take(),
            reported_cdns,
            server,
            compression,
            content_length,
        })
    }
}

/// Formats the declared transfer length without confusing it with decoded or measured bytes.
fn human_content_length(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut scale = 1024_u64;
    let mut unit = "KiB";
    for next in ["MiB", "GiB", "TiB", "PiB", "EiB"] {
        if bytes / scale < 1024 {
            break;
        }
        scale *= 1024;
        unit = next;
    }
    let tenths = (u128::from(bytes) * 10 + u128::from(scale) / 2) / u128::from(scale);
    format!("{}.{} {unit}", tenths / 10, tenths % 10)
}

/// DNS filtering pins actual connections to public addresses; literal checks alone are insufficient.
#[derive(Debug, Default)]
struct PublicResolver {
    inner: DefaultResolver,
    #[cfg(test)]
    allow_loopback: bool,
}

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let resolved = self.inner.resolve(uri, config, timeout)?;
        let mut public = self.empty();
        for address in &resolved {
            #[cfg(test)]
            if self.allow_loopback && address.ip().is_loopback() {
                public.push(*address);
                continue;
            }
            if !ip_address_is_non_public(address.ip()) {
                public.push(*address);
            }
        }
        if public.is_empty() {
            Err(ureq::Error::HostNotFound)
        } else {
            Ok(public)
        }
    }
}

/// Cancellation and the decoded-byte limit are checked between bounded reads.
fn read_body(
    mut reader: impl Read,
    limit: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, Failure> {
    let mut body = Vec::new();
    let mut chunk = [0_u8; 8_192];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        let length = reader.read(&mut chunk).map_err(|_| Failure::Transport)?;
        if length == 0 {
            return Ok(body);
        }
        if body.len().saturating_add(length) > limit {
            return Err(Failure::TooLarge);
        }
        body.extend_from_slice(&chunk[..length]);
    }
}

/// Bounds every line and the combined projection, including all untrusted field values.
#[derive(Default)]
struct Facts {
    lines: Vec<String>,
    bytes: usize,
}

impl Facts {
    fn add(&mut self, label: &str, value: &str) {
        let value = safe_text(value);
        if !value.is_empty() {
            self.line(format!("{label}: {value}"));
        }
    }
    fn line(&mut self, line: String) {
        let line = safe_text(&line);
        if !line.is_empty()
            && self.lines.len() < MAX_FACTS
            && self.bytes.saturating_add(line.len()) <= MAX_FACT_BYTES
        {
            self.bytes += line.len();
            self.lines.push(line);
        }
    }
    fn extend(&mut self, lines: Vec<String>) {
        for line in lines {
            self.line(line);
        }
    }
    fn finish(self) -> Vec<String> {
        self.lines
    }
}

/// Reduces declared text to one bounded, terminal-safe line, excluding embedded markup.
fn safe_text(value: &str) -> String {
    let mut result = String::new();
    let mut space = false;
    let mut tag = false;
    for character in value.chars().take(MAX_FIELD_BYTES * 4) {
        if character == '<' {
            tag = true;
            continue;
        }
        if character == '>' && tag {
            tag = false;
            continue;
        }
        if tag {
            continue;
        }
        if character.is_whitespace() {
            space = !result.is_empty();
            continue;
        }
        if character.is_control()
            || matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        {
            continue;
        }
        if result.len() + usize::from(space) + character.len_utf8() > MAX_FIELD_BYTES {
            break;
        }
        if space {
            result.push(' ');
            space = false;
        }
        result.push(character);
    }
    result
}

/// Preserves the HTML-only test boundary while production resolves declarations against the final URL.
#[cfg(test)]
fn html_facts(html: &[u8]) -> Result<Vec<String>, Failure> {
    metadata::facts(html, None)
}

/// JSON recursion and aggregate node counts are independently bounded after byte-limited parsing.
fn parse_json(bytes: &[u8]) -> Result<Value, Failure> {
    if bytes.len() > MAX_JSON_BYTES {
        return Err(Failure::TooLarge);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Failure::InvalidDocument)?;
    let mut stack = vec![(&value, 0_usize)];
    let mut visited = 0_usize;
    while let Some((node, depth)) = stack.pop() {
        visited += 1;
        if depth > MAX_JSON_DEPTH || visited > MAX_JSON_NODES {
            return Err(Failure::InvalidDocument);
        }
        match node {
            Value::Array(items) => stack.extend(items.iter().map(|item| (item, depth + 1))),
            Value::Object(items) => stack.extend(items.values().map(|item| (item, depth + 1))),
            _ => {}
        }
    }
    Ok(value)
}

/// One validated IANA suffix-to-HTTPS-endpoint mapping.
struct BootstrapService {
    suffix: String,
    endpoint: Url,
}

/// Rejects invalid endpoint data instead of letting registry metadata choose arbitrary transports.
fn parse_bootstrap(value: &Value) -> Result<Vec<BootstrapService>, Failure> {
    let services = value
        .get("services")
        .and_then(Value::as_array)
        .ok_or(Failure::InvalidDocument)?;
    let mut result = Vec::new();
    for service in services.iter().take(MAX_BOOTSTRAP_SUFFIXES) {
        let Some(pair) = service.as_array().filter(|pair| pair.len() == 2) else {
            continue;
        };
        let Some(suffixes) = pair[0].as_array() else {
            continue;
        };
        let Some(endpoints) = pair[1].as_array() else {
            continue;
        };
        let endpoint = endpoints
            .iter()
            .take(8)
            .filter_map(Value::as_str)
            .filter_map(|value| Url::parse(value).ok())
            .find(|url| {
                url.scheme() == "https" && url.query().is_none() && validate_public_url(url).is_ok()
            });
        let Some(endpoint) = endpoint else {
            continue;
        };
        for suffix in suffixes
            .iter()
            .take(MAX_BOOTSTRAP_SUFFIXES)
            .filter_map(Value::as_str)
        {
            let suffix = suffix.trim_end_matches('.').to_ascii_lowercase();
            if valid_domain(&suffix) {
                if result.len() == MAX_BOOTSTRAP_SUFFIXES {
                    return Err(Failure::TooLarge);
                }
                result.push(BootstrapService {
                    suffix,
                    endpoint: endpoint.clone(),
                });
            }
        }
    }
    if result.is_empty() {
        Err(Failure::Unavailable)
    } else {
        Ok(result)
    }
}

/// Domain query labels are ASCII/IDNA hostnames, not URL paths or public-suffix guesses.
fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn check_status(status: u16) -> Result<(), Failure> {
    match status {
        200..=299 => Ok(()),
        404 => Err(Failure::NotFound),
        429 => Err(Failure::RateLimited),
        _ => Err(Failure::Status(status)),
    }
}

/// Projects a matching domain object without following its links or confusing contact roles.
/// Missing fields stay absent; only the server's explicit redaction metadata adds a privacy note.
fn rdap_facts(value: &Value, candidate: &str) -> Result<Vec<String>, Failure> {
    if value.get("objectClassName").and_then(Value::as_str) != Some("domain")
        || !value
            .get("ldhName")
            .and_then(Value::as_str)
            .is_some_and(|name| name.trim_end_matches('.').eq_ignore_ascii_case(candidate))
    {
        return Err(Failure::Mismatch);
    }
    let mut facts = Facts::default();
    if let Some(events) = value.get("events").and_then(Value::as_array) {
        for event in events.iter().take(32) {
            let label = match event.get("eventAction").and_then(Value::as_str) {
                Some("registration") => "Registered",
                Some("last changed") => "Changed",
                Some("expiration") => "Expires",
                _ => continue,
            };
            if let Some(date) = event.get("eventDate").and_then(Value::as_str) {
                facts.add(label, date);
            }
        }
    }
    let mut remaining = 32;
    entity_facts(value.get("entities"), 0, &mut remaining, &mut facts);
    if value
        .get("redacted")
        .and_then(Value::as_array)
        .is_some_and(|fields| !fields.is_empty())
    {
        facts.add(
            "Registration privacy",
            "The server explicitly marks some fields as redacted",
        );
    }
    if let Some(nameservers) = value.get("nameservers").and_then(Value::as_array) {
        let names = nameservers
            .iter()
            .take(16)
            .filter_map(|entry| entry.get("ldhName").and_then(Value::as_str))
            .map(safe_text)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        facts.add("Nameservers", &names);
    }
    if let Some(status) = value.get("status").and_then(Value::as_array) {
        let status = status
            .iter()
            .take(16)
            .filter_map(Value::as_str)
            .map(safe_text)
            .collect::<Vec<_>>()
            .join(", ");
        facts.add("Domain status", &status);
    }
    if let Some(signed) = value
        .get("secureDNS")
        .and_then(|dns| dns.get("delegationSigned"))
        .and_then(Value::as_bool)
    {
        facts.add(
            "DNSSEC",
            if signed {
                "signed delegation (published by registry)"
            } else {
                "unsigned delegation (published by registry)"
            },
        );
    }
    Ok(facts.finish())
}

/// Displays published registrar/registrant names, organizations, and handles with finite traversal.
fn entity_facts(entities: Option<&Value>, depth: usize, remaining: &mut usize, facts: &mut Facts) {
    if depth > 3 {
        return;
    }
    let Some(entities) = entities.and_then(Value::as_array) else {
        return;
    };
    for entity in entities.iter().take(32) {
        if *remaining == 0 {
            return;
        }
        *remaining -= 1;
        let roles = entity.get("roles").and_then(Value::as_array);
        let has_role = |role: &str| {
            roles.is_some_and(|roles| {
                roles
                    .iter()
                    .take(8)
                    .any(|value| value.as_str() == Some(role))
            })
        };
        let is_registrant = has_role("registrant");
        if has_role("registrar") || is_registrant {
            let mut values = Vec::new();
            if let Some(card) = entity
                .get("vcardArray")
                .and_then(Value::as_array)
                .and_then(|card| card.get(1))
                .and_then(Value::as_array)
            {
                for property in card.iter().take(32).filter_map(Value::as_array) {
                    if property.len() < 4 || !matches!(property[0].as_str(), Some("fn" | "org")) {
                        continue;
                    }
                    if let Some(value) = property[3].as_str() {
                        values.push(safe_text(value));
                    } else if let Some(parts) = property[3].as_array() {
                        values.extend(
                            parts
                                .iter()
                                .take(8)
                                .filter_map(Value::as_str)
                                .map(safe_text),
                        );
                    }
                }
            }
            if let Some(handle) = entity.get("handle").and_then(Value::as_str) {
                let handle = safe_text(handle);
                if !handle.is_empty() {
                    values.push(format!("handle {handle}"));
                }
            }
            values.retain(|value| !value.is_empty());
            values.dedup();
            if !values.is_empty() {
                facts.add(
                    if is_registrant {
                        "Registrant (public)"
                    } else {
                        "Registrar"
                    },
                    &values.join("; "),
                );
            }
        }
        entity_facts(entity.get("entities"), depth + 1, remaining, facts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[test]
    fn content_lengths_use_binary_units_without_overflow_or_fabricated_values() {
        for (bytes, expected) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1_048_576, "1.0 MiB"),
            (u64::MAX, "16.0 EiB"),
        ] {
            assert_eq!(human_content_length(bytes), expected);
        }
        assert!(HttpResponse::default().content_length.is_none());
    }

    /// Independent post-response lookups overlap and enrich the same page without forwarding its URL.
    #[test]
    fn ip_registration_and_server_release_lookups_overlap() {
        use std::sync::mpsc::{Receiver, Sender, channel};
        struct Parallel {
            network_started: Sender<()>,
            release_started: Sender<()>,
            network_wait: Mutex<Receiver<()>>,
            release_wait: Mutex<Receiver<()>>,
        }
        impl HttpTransport for Parallel {
            fn fetch(
                &self,
                url: &Url,
                _: DocumentKind,
                _: Duration,
                _: &AtomicBool,
            ) -> Result<HttpResponse, Failure> {
                match url.as_str() {
                    "https://8.8.8.8/private?q=secret" => Ok(HttpResponse {
                        status: 200,
                        content_type: "text/html".into(),
                        body: b"<title>Preserved</title>".to_vec(),
                        peer_ip: Some("8.8.8.8".parse().unwrap()),
                        server: "nginx/1.24.0".into(),
                        ..HttpResponse::default()
                    }),
                    "https://data.iana.org/rdap/ipv4.json" => {
                        self.network_started.send(()).unwrap();
                        self.release_wait
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap();
                        Err(Failure::Unavailable)
                    }
                    "https://nginx.org/en/CHANGES-1.24" => {
                        self.release_started.send(()).unwrap();
                        self.network_wait
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap();
                        Ok(HttpResponse {
                            status: 200,
                            content_type: "text/plain".into(),
                            body: b"Changes with nginx 1.24.0 11 Apr 2023\n".to_vec(),
                            ..HttpResponse::default()
                        })
                    }
                    _ => panic!("unexpected service request"),
                }
            }
        }
        let (network_started, network_wait) = channel();
        let (release_started, release_wait) = channel();
        let transport = Parallel {
            network_started,
            release_started,
            network_wait: Mutex::new(network_wait),
            release_wait: Mutex::new(release_wait),
        };
        let facts = UrlInfoClient::default().lookup_with(
            &Url::parse("https://8.8.8.8/private?q=secret").unwrap(),
            &AtomicBool::new(false),
            &transport,
        );
        assert!(facts.contains(&"Title: Preserved".into()));
        assert!(
            facts.contains(&"Server version released (upstream): nginx 1.24.0 - April 2023".into())
        );
        assert!(facts.contains(&"Server release source: https://nginx.org/en/CHANGES-1.24".into()));
    }

    /// Peer observations belong to the final page, never a second DNS lookup or registry socket.
    #[test]
    fn website_connection_facts_include_only_supplied_headers_and_verified_peer() {
        struct Observed;
        impl HttpTransport for Observed {
            fn fetch(
                &self,
                url: &Url,
                _: DocumentKind,
                _: Duration,
                _: &AtomicBool,
            ) -> Result<HttpResponse, Failure> {
                if url.as_str() != "https://8.8.8.8/" {
                    return Err(Failure::Unavailable);
                }
                Ok(HttpResponse {
                    status: 200,
                    content_type: "text/html".into(),
                    body: b"<title>A page</title>".to_vec(),
                    peer_ip: Some("8.8.8.8".parse().unwrap()),
                    server: "nginx/1.26.3".into(),
                    compression: "gzip".into(),
                    content_length: Some(25_190),
                    reported_cdns: vec!["Cloudflare"],
                    tls_info: Some(tls::TlsInfo {
                        facts: vec!["TLS version: 1.3".into()],
                    }),
                    ..HttpResponse::default()
                })
            }
        }
        let facts = UrlInfoClient::default().lookup_with(
            &Url::parse("https://8.8.8.8/").unwrap(),
            &AtomicBool::new(false),
            &Observed,
        );
        for expected in [
            "Title: A page",
            "IP address: 8.8.8.8",
            "Server: nginx/1.26.3",
            "Compression: gzip",
            "Content length: 24.6 KiB",
            "CDN (reported): Cloudflare",
            "TLS version: 1.3",
        ] {
            assert!(
                facts.iter().any(|fact| fact == expected),
                "missing {expected}: {facts:?}"
            );
        }
        assert!(!facts.iter().any(|fact| fact.starts_with("Network country")));
    }

    /// Scripted per-URL queues preserve redirect order without depending on branch scheduling.
    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<VecDeque<(String, Result<HttpResponse, Failure>)>>,
        requests: Mutex<Vec<String>>,
    }

    impl MockTransport {
        fn push(&self, url: &str, status: u16, body: impl Into<Vec<u8>>) {
            self.responses.lock().unwrap().push_back((
                url.to_owned(),
                Ok(HttpResponse {
                    status,
                    location: None,
                    content_type: if body_is_json_url(url) {
                        "application/rdap+json"
                    } else {
                        "text/html"
                    }
                    .to_owned(),
                    body: body.into(),
                    ..HttpResponse::default()
                }),
            ));
        }

        fn redirect(&self, url: &str, location: &str) {
            self.responses.lock().unwrap().push_back((
                url.to_owned(),
                Ok(HttpResponse {
                    status: 302,
                    location: Some(location.to_owned()),
                    content_type: String::new(),
                    body: Vec::new(),
                    ..HttpResponse::default()
                }),
            ));
        }
    }

    /// Fixture endpoints ending in JSON or containing domain/ return registration documents.
    fn body_is_json_url(url: &str) -> bool {
        url.ends_with(".json") || url.contains("/domain/")
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
            let mut responses = self.responses.lock().unwrap();
            let index = responses
                .iter()
                .position(|(expected, _)| expected == url.as_str())
                .expect("unexpected request");
            responses.remove(index).expect("matched response").1
        }
    }

    /// Default bootstrap fixtures deliberately use a multi-label registry suffix.
    fn bootstrap() -> Vec<u8> {
        serde_json::to_vec(&json!({ "services": [
            [["uk"], ["https://general.example/rdap/"]],
            [["co.uk"], ["https://registry.example/rdap/"]]
        ]}))
        .unwrap()
    }

    /// Domain payloads must identify the exact candidate being queried.
    fn domain(name: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({ "objectClassName": "domain", "ldhName": name })).unwrap()
    }

    /// Metadata lookups never contact private endpoints or URL-embedded credentials.
    #[test]
    fn public_url_validation_rejects_private_addresses_credentials_and_ports() {
        for raw in [
            "https://example.com/",
            "http://example.com:80/page#fragment",
            "https://example.com:443/page",
            "https://example.com/?q=ordinary&lang=en",
        ] {
            assert!(
                validate_public_url(&Url::parse(raw).unwrap()).is_ok(),
                "{raw}"
            );
        }
        for raw in [
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://10.0.0.1/",
            "http://169.254.169.254/",
            "http://localhost/",
            "http://host.internal/",
            "http://host.local/",
            "https://user:secret@example.com/",
            "https://example.com:8443/",
            "file:///etc/passwd",
            "ftp://example.com/file",
            "http://printer/",
        ] {
            assert!(
                validate_public_url(&Url::parse(raw).unwrap()).is_err(),
                "{raw}"
            );
            let transport = MockTransport::default();
            let facts = UrlInfoClient::default().lookup_with(
                &Url::parse(raw).unwrap(),
                &AtomicBool::new(false),
                &transport,
            );
            assert!(transport.requests.lock().unwrap().is_empty());
            assert!(!facts.join("\n").contains("secret"));
        }
    }

    /// HTML declarations are parsed as data, with standard description taking precedence.
    #[test]
    fn html_metadata_ignores_scripts_body_and_templates_and_decodes_entities() {
        let facts = html_facts(
            br#"<!doctype html><html lang='en'><head>
			<title> Artist &amp; Friends </title>
			<script>document.write('<meta name="description" content="script secret">')</script>
			<template><meta name='author' content='template secret'></template>
			<meta property='og:description' content='Fallback'>
			<meta NAME='description' content='A &amp; B'>
			<meta name='author' content='The artist'><meta property='og:site_name' content='Artist site'>
			</head><body><meta name='description' content='body secret'></body></html>"#,
        )
        .unwrap();
        assert!(facts.contains(&"Title: Artist & Friends".to_owned()));
        assert!(facts.contains(&"Description: A & B".to_owned()));
        assert!(facts.contains(&"Author (website claim): The artist".to_owned()));
        assert!(facts.contains(&"Site: Artist site".to_owned()));
        assert!(facts.contains(&"Language: en".to_owned()));
        assert!(!facts.join("\n").contains("secret"));
        assert_eq!(
            html_facts(b"<meta property='og:description' content='Fallback'>").unwrap(),
            ["Description: Fallback"]
        );
        assert_eq!(
            html_facts(
                br#"<meta name="description" content="&quot;A &amp; B&quot; &#39;quoted&#39;">"#
            )
            .unwrap(),
            ["Description: \"A & B\" 'quoted'"]
        );
    }

    /// Page prose uses ASCII dashes in TTYs, including entity-decoded Open Graph fallbacks.
    #[test]
    fn html_title_and_description_use_ascii_dashes() {
        for html in [
            "<title>Предание.ру — помощь &mdash; людям</title>\
             <meta name='description' content='Музыка — книги &#8212; видео'>\
             <meta property='og:site_name' content='Сайт — имя'>",
            "<meta property='og:title' content='Предание.ру &mdash; помощь — людям'>\
             <meta property='og:description' content='Музыка &#x2014; книги — видео'>\
             <meta property='og:site_name' content='Сайт — имя'>",
        ] {
            assert_eq!(
                html_facts(html.as_bytes()).unwrap(),
                [
                    "Title: Предание.ру - помощь - людям",
                    "Description: Музыка - книги - видео",
                    "Site: Сайт — имя",
                ]
            );
        }
    }

    /// Each request must observe the other before completing, not merely run in a new order.
    #[test]
    fn website_and_registration_requests_overlap_with_cold_and_warm_bootstrap() {
        use std::sync::{Mutex, mpsc};

        struct OverlapTransport {
            html_started: mpsc::SyncSender<()>,
            html_receiver: Mutex<mpsc::Receiver<()>>,
            rdap_started: mpsc::SyncSender<()>,
            rdap_receiver: Mutex<mpsc::Receiver<()>>,
            html_overlapped: AtomicBool,
            rdap_overlapped: AtomicBool,
            html_failure: Option<Failure>,
            rdap_failure: Option<Failure>,
        }

        impl HttpTransport for OverlapTransport {
            fn fetch(
                &self,
                url: &Url,
                kind: DocumentKind,
                _: Duration,
                _: &AtomicBool,
            ) -> Result<HttpResponse, Failure> {
                let body = if matches!(kind, DocumentKind::Html) {
                    self.html_started.send(()).unwrap();
                    self.html_overlapped.store(
                        self.rdap_receiver
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(2))
                            .is_ok(),
                        Ordering::Relaxed,
                    );
                    if let Some(error) = self.html_failure {
                        return Err(error);
                    }
                    b"<title>Concurrent page</title>".to_vec()
                } else if url.as_str() == BOOTSTRAP_URL {
                    bootstrap()
                } else {
                    self.rdap_started.send(()).unwrap();
                    self.rdap_overlapped.store(
                        self.html_receiver
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(2))
                            .is_ok(),
                        Ordering::Relaxed,
                    );
                    if let Some(error) = self.rdap_failure {
                        return Err(error);
                    }
                    serde_json::to_vec(&json!({
                        "objectClassName": "domain", "ldhName": "artist.co.uk",
                        "events": [{"eventAction": "registration", "eventDate": "2000-01-01T00:00:00Z"}]
                    }))
                    .unwrap()
                };
                Ok(HttpResponse {
                    status: 200,
                    location: None,
                    content_type: if matches!(kind, DocumentKind::Html) {
                        "text/html"
                    } else {
                        "application/rdap+json"
                    }
                    .into(),
                    body,
                    ..HttpResponse::default()
                })
            }
        }

        for (cached, html_failure, rdap_failure) in [false, true].into_iter().flat_map(|cached| {
            [
                (cached, None, None),
                (cached, Some(Failure::Timeout), None),
                (cached, None, Some(Failure::Timeout)),
            ]
        }) {
            let (html_started, html_receiver) = mpsc::sync_channel(1);
            let (rdap_started, rdap_receiver) = mpsc::sync_channel(1);
            let transport = OverlapTransport {
                html_started,
                html_receiver: Mutex::new(html_receiver),
                rdap_started,
                rdap_receiver: Mutex::new(rdap_receiver),
                html_overlapped: AtomicBool::new(false),
                rdap_overlapped: AtomicBool::new(false),
                html_failure,
                rdap_failure,
            };
            let mut client = UrlInfoClient::default();
            if cached {
                client.bootstrap =
                    Some(parse_bootstrap(&parse_json(&bootstrap()).unwrap()).unwrap());
            }
            let facts = client.lookup_with(
                &Url::parse("https://artist.co.uk/").unwrap(),
                &AtomicBool::new(false),
                &transport,
            );
            assert!(transport.html_overlapped.load(Ordering::Relaxed));
            assert!(transport.rdap_overlapped.load(Ordering::Relaxed));
            let mut expected = if html_failure.is_some() {
                vec!["Website: The URL information time limit was reached"]
            } else {
                vec!["Website response: HTTP 200", "Title: Concurrent page"]
            };
            if rdap_failure.is_some() {
                expected.push("Registration: The URL information time limit was reached");
            } else {
                expected.extend([
                    "Registered: 2000-01-01T00:00:00Z",
                    "RDAP source: https://registry.example/rdap/domain/artist.co.uk",
                ]);
            }
            assert_eq!(facts, expected);
        }
    }

    /// Unchanged destinations do not repeat the URL, including after round-trip redirects.
    #[test]
    fn unchanged_final_url_is_omitted_without_losing_website_or_registration_facts() {
        for (original, requested) in [
            ("https://artist.co.uk/music", "https://artist.co.uk/music"),
            (
                "https://ARTIST.CO.UK:443/music#section",
                "https://artist.co.uk/music",
            ),
            (
                "https://artist.co.uk/music?q=one%20two&lang=en#section",
                "https://artist.co.uk/music?q=one%20two&lang=en",
            ),
        ] {
            for round_trip in [false, true] {
                let transport = MockTransport::default();
                if round_trip {
                    transport.redirect(requested, "/intermediate");
                    transport.redirect("https://artist.co.uk/intermediate", requested);
                }
                transport.push(requested, 200, b"<title>Artist</title>".to_vec());
                transport.push(BOOTSTRAP_URL, 200, bootstrap());
                transport.push(
                    "https://registry.example/rdap/domain/artist.co.uk",
                    200,
                    domain("artist.co.uk"),
                );
                let facts = UrlInfoClient::default().lookup_with(
                    &Url::parse(original).unwrap(),
                    &AtomicBool::new(false),
                    &transport,
                );
                assert!(
                    !facts.iter().any(|line| line.starts_with("Final URL:")),
                    "unchanged destination should be hidden: {facts:?}"
                );
                assert!(facts.contains(&"Website response: HTTP 200".to_owned()));
                assert!(facts.contains(&"Title: Artist".to_owned()));
                assert!(facts.contains(
                    &"RDAP source: https://registry.example/rdap/domain/artist.co.uk".to_owned()
                ));
                assert_eq!(
                    transport.requests.lock().unwrap().len(),
                    if round_trip { 5 } else { 3 }
                );
                assert!(transport.responses.lock().unwrap().is_empty());
            }
        }
    }

    /// A query-only redirect is meaningful even when the origin and path stay unchanged.
    #[test]
    fn query_only_redirect_keeps_the_final_url() {
        let transport = MockTransport::default();
        transport.redirect("https://artist.co.uk/music?q=old", "?q=new#section");
        transport.push(
            "https://artist.co.uk/music?q=new",
            200,
            b"<title>New results</title>".to_vec(),
        );
        transport.push(BOOTSTRAP_URL, 200, bootstrap());
        transport.push(
            "https://registry.example/rdap/domain/artist.co.uk",
            200,
            domain("artist.co.uk"),
        );
        let facts = UrlInfoClient::default().lookup_with(
            &Url::parse("https://artist.co.uk/music?q=old#section").unwrap(),
            &AtomicBool::new(false),
            &transport,
        );
        assert!(facts.contains(&"Final URL: https://artist.co.uk/music?q=new".to_owned()));
        assert!(facts.contains(&"Title: New results".to_owned()));
        assert!(transport.responses.lock().unwrap().is_empty());
    }

    /// Explicit website queries survive redirects but never reach IANA or RDAP.
    #[test]
    fn website_queries_reach_only_the_website_not_registration_services() {
        let transport = MockTransport::default();
        let url =
            Url::parse("https://artist.co.uk/music?q=one%20two&lang=en#private-fragment").unwrap();
        transport.redirect(
            "https://artist.co.uk/music?q=one%20two&lang=en",
            "/results?q=one%20two&lang=en",
        );
        transport.push(
            "https://artist.co.uk/results?q=one%20two&lang=en",
            200,
            b"<title>Search results</title>".to_vec(),
        );
        transport.push(BOOTSTRAP_URL, 200, bootstrap());
        transport.push(
            "https://registry.example/rdap/domain/artist.co.uk",
            200,
            domain("artist.co.uk"),
        );
        let facts = UrlInfoClient::default()
            .lookup_with(&url, &AtomicBool::new(false), &transport)
            .join("\n");
        assert!(facts.contains("Title: Search results"));
        assert!(!facts.contains("Domain:"));
        assert!(!facts.contains("Registered domain:"));
        assert!(facts.contains("Final URL: https://artist.co.uk/results?q=one%20two&lang=en"));
        assert!(!facts.contains("private-fragment"));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let registration_requests = requests
            .iter()
            .filter(|request| Url::parse(request).unwrap().host_str() != Some("artist.co.uk"))
            .collect::<Vec<_>>();
        assert_eq!(registration_requests.len(), 2);
        assert!(
            registration_requests
                .iter()
                .all(|request| !request.contains('?') && !request.contains("one%20two"))
        );
        assert!(transport.responses.lock().unwrap().is_empty());
    }

    /// Redirected pages keep registration attached to the original normalized hostname.
    #[test]
    fn lookup_only_labels_registered_parents_of_the_original_hostname() {
        for (original, destination, parent) in [
            (
                "https://ARTIST.CO.UK./music",
                "https://destination.example/final",
                false,
            ),
            (
                "https://www.artist.co.uk/music",
                "https://artist.co.uk/final",
                true,
            ),
        ] {
            let transport = MockTransport::default();
            let url = Url::parse(original).unwrap();
            transport.redirect(url.as_str(), destination);
            transport.push(
                destination,
                200,
                b"<title>Redirected artist</title>".to_vec(),
            );
            transport.push(BOOTSTRAP_URL, 200, bootstrap());
            if parent {
                transport.push(
                    "https://registry.example/rdap/domain/www.artist.co.uk",
                    404,
                    Vec::new(),
                );
            }
            transport.push(
                "https://registry.example/rdap/domain/artist.co.uk",
                200,
                serde_json::to_vec(&json!({
                    "objectClassName": "domain", "ldhName": "ARTIST.CO.UK.",
                    "events": [{"eventAction": "registration", "eventDate": "2000-01-01T00:00:00Z"}]
                }))
                .unwrap(),
            );
            let facts =
                UrlInfoClient::default().lookup_with(&url, &AtomicBool::new(false), &transport);
            assert!(facts.contains(&"Title: Redirected artist".to_owned()));
            assert!(facts.contains(&format!("Final URL: {destination}")));
            assert!(facts.contains(&"Registered: 2000-01-01T00:00:00Z".to_owned()));
            assert!(facts.contains(
                &"RDAP source: https://registry.example/rdap/domain/artist.co.uk".to_owned()
            ));
            assert_eq!(
                facts.contains(&"Registered domain: artist.co.uk".to_owned()),
                parent
            );
            for omitted in ["Transport:", "Domain:", "Registrant:", "Registration note:"] {
                assert!(
                    !facts.iter().any(|line| line.starts_with(omitted)),
                    "{facts:?}"
                );
            }
            assert!(transport.responses.lock().unwrap().is_empty());
        }
    }

    /// All rendered text has finite size and cannot inject terminal controls or bidi overrides.
    #[test]
    fn returned_text_and_documents_are_bounded() {
        let text = safe_text(&format!(
            " \u{1b}[31mA\nB\t\u{202e}{}",
            "x".repeat(MAX_FIELD_BYTES * 2)
        ));
        assert!(text.len() <= MAX_FIELD_BYTES);
        assert!(!text.chars().any(char::is_control));
        assert!(!text.contains('\u{202e}'));
        assert!(html_facts(&vec![b'x'; MAX_HTML_BYTES + 1]).is_err());
        assert!(parse_json(&vec![b' '; MAX_JSON_BYTES + 1]).is_err());
        let nested = format!(
            "{}0{}",
            "[".repeat(MAX_JSON_DEPTH + 1),
            "]".repeat(MAX_JSON_DEPTH + 1)
        );
        assert!(parse_json(nested.as_bytes()).is_err());
        let mut facts = Facts::default();
        for _ in 0..1_000 {
            facts.add("Value", &"x".repeat(MAX_FIELD_BYTES * 2));
        }
        let lines = facts.finish();
        assert!(lines.len() <= MAX_FACTS);
        assert!(lines.iter().map(String::len).sum::<usize>() <= MAX_FACT_BYTES);
    }

    /// Public registrant fields are labeled separately from registrar identity and redaction.
    #[test]
    fn rdap_fields_are_role_aware_bounded_and_match_the_requested_domain() {
        let value = json!({
            "objectClassName": "domain", "ldhName": "EXAMPLE.COM",
            "events": [{"eventAction":"registration", "eventDate":"2000-01-01T00:00:00Z"},
                {"eventAction":"last changed", "eventDate":"2025-01-01T00:00:00Z"},
                {"eventAction":"expiration", "eventDate":"2030-01-01T00:00:00Z"}],
            "entities": [
                {"roles":["registrar"], "handle":"REG-1", "vcardArray":["vcard", [["fn", {}, "text", "Registry Company"]]]},
                {"roles":["registrant"], "vcardArray":["vcard", [["org", {}, "text", ["Public Organization"]]]]},
                {"roles":["technical"], "vcardArray":["vcard", [["fn", {}, "text", "Not an owner"]]]}
            ],
            "nameservers":[{"ldhName":"ns1.example.com"}], "status":["active"],
            "secureDNS":{"delegationSigned":true}, "redacted":[{"name":{"type":"Registrant Name"}}]
        });
        let facts = rdap_facts(&value, "example.com").unwrap().join("\n");
        for expected in [
            "Registered: 2000",
            "Changed: 2025",
            "Expires: 2030",
            "Registrar: Registry Company",
            "REG-1",
            "Registrant (public): Public Organization",
            "Nameservers: ns1.example.com",
            "DNSSEC: signed",
            "redact",
        ] {
            assert!(facts.contains(expected), "missing {expected}: {facts}");
        }
        assert!(!facts.contains("Not an owner"));
        assert!(rdap_facts(&value, "other.com").is_err());
        assert!(
            rdap_facts(
                &json!({"objectClassName":"entity","ldhName":"example.com"}),
                "example.com"
            )
            .is_err()
        );
        let absent = rdap_facts(
            &json!({"objectClassName":"domain","ldhName":"example.com"}),
            "example.com",
        )
        .unwrap()
        .join("\n");
        assert!(
            absent.is_empty(),
            "absent metadata must not produce boilerplate: {absent}"
        );
    }

    /// Missing contact fields stay silent while public identities and explicit redaction survive.
    #[test]
    fn rdap_contacts_omit_empty_placeholders_and_keep_published_details() {
        for entities in [
            Value::Null,
            json!([]),
            json!([{"roles": ["registrant"]}]),
            json!([{"roles": ["registrant"], "vcardArray": ["vcard", [
				["fn", {}, "text", "  "], ["org", {}, "text", ["", "\n"]],
				["fn", {}, "text"], ["org", {}, "text", {}]
			]], "handle": "\t"}]),
        ] {
            let value = json!({"objectClassName": "domain", "ldhName": "example.com", "entities": entities});
            assert!(rdap_facts(&value, "example.com").unwrap().is_empty());
        }
        let value = json!({
            "objectClassName": "domain", "ldhName": "example.com",
            "entities": [{"roles": ["registrar"], "entities": [
                {"roles": ["registrant"], "vcardArray": ["vcard", [["fn", {}, "text", "Public Name"]]]},
                {"roles": ["registrant"], "handle": "PUBLIC-1"}
            ]}],
            "redacted": [{"name": {"type": "Registrant Email"}}]
        });
        assert_eq!(
            rdap_facts(&value, "example.com").unwrap(),
            [
                "Registrant (public): Public Name",
                "Registrant (public): handle PUBLIC-1",
                "Registration privacy: The server explicitly marks some fields as redacted",
            ]
        );
    }

    /// Parent retries occur only after authoritative 404s and bootstrap data is session-cached.
    #[test]
    fn lookup_uses_longest_bootstrap_suffix_retries_404_and_reuses_bootstrap() {
        let transport = MockTransport::default();
        let url = Url::parse("https://www.artist.co.uk/music").unwrap();
        transport.push(url.as_str(), 200, b"<title>Artist</title>".to_vec());
        transport.push(BOOTSTRAP_URL, 200, bootstrap());
        transport.push(
            "https://registry.example/rdap/domain/www.artist.co.uk",
            404,
            Vec::new(),
        );
        transport.push(
            "https://registry.example/rdap/domain/artist.co.uk",
            200,
            domain("artist.co.uk"),
        );
        let mut client = UrlInfoClient::default();
        let facts = client
            .lookup_with(&url, &AtomicBool::new(false), &transport)
            .join("\n");
        assert!(facts.contains("Title: Artist"));
        assert!(facts.contains("Registered domain: artist.co.uk"));
        assert!(!facts.contains("Registration note:"));
        transport.push(url.as_str(), 200, Vec::new());
        transport.push(
            "https://registry.example/rdap/domain/www.artist.co.uk",
            200,
            domain("www.artist.co.uk"),
        );
        client.lookup_with(&url, &AtomicBool::new(false), &transport);
        assert!(transport.responses.lock().unwrap().is_empty());
        assert_eq!(
            transport
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|url| *url == BOOTSTRAP_URL)
                .count(),
            1
        );
    }

    /// Rate limits, mismatched objects, and transport errors never probe a parent domain.
    #[test]
    fn rdap_failure_preserves_website_facts_without_parent_guessing() {
        for (status, body) in [
            (429, Vec::new()),
            (503, Vec::new()),
            (200, domain("other.co.uk")),
        ] {
            let transport = MockTransport::default();
            transport.push(
                "https://www.artist.co.uk/",
                200,
                b"<meta name='description' content='Page survives'>".to_vec(),
            );
            transport.push(BOOTSTRAP_URL, 200, bootstrap());
            transport.push(
                "https://registry.example/rdap/domain/www.artist.co.uk",
                status,
                body,
            );
            let facts = UrlInfoClient::default()
                .lookup_with(
                    &Url::parse("https://www.artist.co.uk/").unwrap(),
                    &AtomicBool::new(false),
                    &transport,
                )
                .join("\n");
            assert!(facts.contains("Description: Page survives"));
            assert!(facts.contains("Registration:"));
            assert!(!facts.contains("Registered domain:"));
            assert!(!facts.contains("Transport:"));
            assert_eq!(transport.requests.lock().unwrap().len(), 3);
        }
    }

    /// Website failures retain useful registration facts without replacing omissions with notices.
    #[test]
    fn website_failure_preserves_registration_facts_without_boilerplate() {
        let transport = MockTransport::default();
        let url = Url::parse("https://artist.co.uk/").unwrap();
        transport
            .responses
            .lock()
            .unwrap()
            .push_back((url.to_string(), Err(Failure::Transport)));
        transport.push(BOOTSTRAP_URL, 200, bootstrap());
        transport.push(
            "https://registry.example/rdap/domain/artist.co.uk",
            200,
            serde_json::to_vec(&json!({
                "objectClassName": "domain", "ldhName": "artist.co.uk",
                "events": [{"eventAction": "registration", "eventDate": "2000-01-01T00:00:00Z"}]
            }))
            .unwrap(),
        );
        let facts = UrlInfoClient::default().lookup_with(&url, &AtomicBool::new(false), &transport);
        assert_eq!(
            facts,
            [
                "Website: Could not connect to the public server",
                "Registered: 2000-01-01T00:00:00Z",
                "RDAP source: https://registry.example/rdap/domain/artist.co.uk",
            ]
        );
        assert!(transport.responses.lock().unwrap().is_empty());
    }

    /// Redirect policy is enforced before any subsequent request, even with an injected transport.
    #[test]
    fn redirects_reject_private_credential_and_downgrade_targets() {
        for target in [
            "http://127.0.0.1/",
            "https://user:pass@example.com/",
            "http://public.example/",
        ] {
            let transport = MockTransport::default();
            transport.redirect("https://artist.example/", target);
            let cancelled = AtomicBool::new(false);
            let budget = Budget::new(&cancelled);
            assert!(
                fetch_document(
                    &transport,
                    &Url::parse("https://artist.example/").unwrap(),
                    DocumentKind::Html,
                    &budget
                )
                .is_err()
            );
            assert_eq!(transport.requests.lock().unwrap().len(), 1);
        }
    }

    /// Resource limits stop both redirect chains and already-cancelled lookups without extra I/O.
    #[test]
    fn redirect_request_and_cancellation_budgets_are_finite() {
        let transport = MockTransport::default();
        for index in 0..=MAX_REDIRECTS {
            transport.redirect(
                &format!("https://artist.example/{index}"),
                &format!("/{next}", next = index + 1),
            );
        }
        let cancelled = AtomicBool::new(false);
        let mut budget = Budget::new(&cancelled);
        assert_eq!(
            fetch_document(
                &transport,
                &Url::parse("https://artist.example/0").unwrap(),
                DocumentKind::Html,
                &budget
            )
            .unwrap_err(),
            Failure::Redirect
        );
        assert_eq!(transport.requests.lock().unwrap().len(), MAX_REDIRECTS + 1);
        budget.remaining_requests.store(0, Ordering::Relaxed);
        assert_eq!(budget.request().unwrap_err(), Failure::Budget);
        budget.deadline = Instant::now();
        assert_eq!(budget.remaining().unwrap_err(), Failure::Timeout);
        let unused = MockTransport::default();
        UrlInfoClient::default().lookup_with(
            &Url::parse("https://artist.example/").unwrap(),
            &AtomicBool::new(true),
            &unused,
        );
        assert!(unused.requests.lock().unwrap().is_empty());
    }

    /// Website redirects expose only their validated final destination and actual HTTP status.
    #[test]
    fn website_redirects_preserve_facts_when_no_rdap_service_exists() {
        let transport = MockTransport::default();
        transport.redirect(
            "http://artist.example/start",
            "https://artist.example/final",
        );
        transport.push(
            "https://artist.example/final",
            200,
            b"<title>Redirected title</title>".to_vec(),
        );
        transport.push(BOOTSTRAP_URL, 200, bootstrap());
        transport.push(
            "https://www.iana.org/whois?q=example",
            200,
            b"<pre>domain: EXAMPLE</pre>".to_vec(),
        );
        let facts = UrlInfoClient::default()
            .lookup_with(
                &Url::parse("http://artist.example/start#private-fragment").unwrap(),
                &AtomicBool::new(false),
                &transport,
            )
            .join("\n");
        assert!(facts.contains("Website response: HTTP 200"));
        assert!(facts.contains("Final URL: https://artist.example/final"));
        assert!(facts.contains(
            "Redirect chain: http://artist.example/start -> https://artist.example/final"
        ));
        assert!(!facts.contains("Transport:"));
        assert!(facts.contains("Title: Redirected title"));
        assert!(facts.contains("No supported registration service"));
        assert!(!facts.contains("private-fragment"));
    }

    /// Invalid bootstrap endpoints cannot become fetch targets and RDAP never guesses beyond its cap.
    #[test]
    fn bootstrap_endpoint_policy_and_domain_attempt_bounds_are_enforced() {
        for endpoint in [
            "http://registry.example/",
            "https://127.0.0.1/",
            "https://registry.example/?secret=x",
            "https://registry.example:8443/",
        ] {
            assert!(parse_bootstrap(&json!({"services":[[["com"],[endpoint]]]})).is_err());
        }
        for (host, attempts) in [("a.b.c.d.e.f.co.uk", 5), ("artist.co.uk", 2)] {
            let transport = MockTransport::default();
            let original = format!("https://{host}/");
            transport.push(&original, 503, Vec::new());
            transport.push(BOOTSTRAP_URL, 200, bootstrap());
            let mut candidate = host;
            for _ in 0..attempts {
                transport.push(
                    &format!("https://registry.example/rdap/domain/{candidate}"),
                    404,
                    Vec::new(),
                );
                candidate = candidate.split_once('.').unwrap().1;
            }
            let facts = UrlInfoClient::default()
                .lookup_with(
                    &Url::parse(&original).unwrap(),
                    &AtomicBool::new(false),
                    &transport,
                )
                .join("\n");
            assert!(facts.contains("No matching registration record"));
            assert_eq!(transport.requests.lock().unwrap().len(), attempts + 2);
            assert!(transport.responses.lock().unwrap().is_empty());
        }
    }

    /// Byte limits apply to chunked/unknown-size bodies and cancellation also covers body reads.
    #[test]
    fn body_reading_rejects_overflow_cancellation_and_io_errors() {
        let cancelled = AtomicBool::new(false);
        assert_eq!(read_body(&b"exact"[..], 5, &cancelled).unwrap(), b"exact");
        assert_eq!(
            read_body(&b"overflow"[..], 5, &cancelled).unwrap_err(),
            Failure::TooLarge
        );
        assert_eq!(
            read_body(&b"unused"[..], 5, &AtomicBool::new(true)).unwrap_err(),
            Failure::Cancelled
        );
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("private transport details"))
            }
        }
        let error = read_body(Broken, 5, &cancelled).unwrap_err();
        assert_eq!(error, Failure::Transport);
        assert!(!error.message().contains("private"));
    }

    /// Large websites retain bounded head metadata; registration JSON is never truncated.
    #[test]
    fn http_transport_keeps_large_html_head_but_rejects_oversized_json() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::thread;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            for index in 0..3 {
                let (mut stream, _) = loop {
                    assert!(Instant::now() < deadline, "mock large-document deadline");
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("mock accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 8_192);
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let headers = match index {
                    0 => format!(
                        "Content-Type: text/html\r\nContent-Length: {}\r\n",
                        MAX_HTML_BYTES * 2
                    ),
                    1 => "Content-Type: text/html\r\n".into(),
                    _ => format!(
                        "Content-Type: application/rdap+json\r\nContent-Length: {}\r\n",
                        MAX_JSON_BYTES + 1
                    ),
                };
                stream
                    .write_all(
                        format!("HTTP/1.1 200 OK\r\n{headers}Connection: close\r\n\r\n").as_bytes(),
                    )
                    .unwrap();
                if index < 2 {
                    let mut body = b"<head><title>Large page</title><meta name='description' content='Useful head'></head><body>".to_vec();
                    // The known-size response intentionally omits its tail. A prefix
                    // lookup succeeds without waiting for the remaining advertised bytes.
                    body.resize(MAX_HTML_BYTES + usize::from(index == 1), b'x');
                    let _ = stream.write_all(&body);
                }
            }
        });
        let url = Url::parse(&format!("http://{address}/page")).unwrap();
        let transport = UreqTransport {
            allow_loopback: true,
        };
        let cancelled = AtomicBool::new(false);
        for _ in 0..2 {
            let response = transport
                .fetch(&url, DocumentKind::Html, Duration::from_secs(2), &cancelled)
                .unwrap();
            assert_eq!(response.body.len(), MAX_HTML_BYTES);
            assert_eq!(
                html_facts(&response.body).unwrap(),
                ["Title: Large page", "Description: Useful head"]
            );
        }
        assert_eq!(
            transport
                .fetch(&url, DocumentKind::Json, Duration::from_secs(2), &cancelled)
                .unwrap_err(),
            Failure::TooLarge
        );
        server.join().unwrap();
    }

    /// A real local HTTP fixture is reachable only through the test-only transport exception.
    #[test]
    fn http_transport_never_replays_cookie_authorization_or_referer_headers() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::thread;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = Vec::new();
            while requests.len() < 2 {
                assert!(Instant::now() < deadline, "mock HTTP request deadline");
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 8_192);
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                requests.push(String::from_utf8(request).unwrap());
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nServer: nginx/1.26.3\r\nContent-Encoding: identity\r\nSet-Cookie: secret=must-not-replay\r\nContent-Length: 16\r\nConnection: close\r\n\r\n<title>x</title>").unwrap();
            }
            requests
        });
        let url = Url::parse(&format!("http://{address}/page")).unwrap();
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            UreqTransport::default()
                .fetch(&url, DocumentKind::Html, Duration::from_secs(1), &cancelled)
                .unwrap_err(),
            Failure::InvalidUrl
        );
        let transport = UreqTransport {
            allow_loopback: true,
        };
        for _ in 0..2 {
            let response = transport
                .fetch(&url, DocumentKind::Html, Duration::from_secs(2), &cancelled)
                .unwrap();
            assert_eq!(response.body, b"<title>x</title>");
            assert_eq!(response.peer_ip, Some(address.ip()));
            assert_eq!(response.server, "nginx/1.26.3");
            assert_eq!(response.content_length, Some(16));
            assert!(response.compression.is_empty());
            assert!(response.reported_cdns.is_empty());
            assert!(response.tls_info.is_none());
        }
        for request in server.join().unwrap() {
            let request = request.to_ascii_lowercase();
            for header in [
                "\r\ncookie:",
                "\r\nauthorization:",
                "\r\nproxy-authorization:",
                "\r\nreferer:",
            ] {
                assert!(!request.contains(header));
            }
        }
    }

    /// Concurrent branches share a single allowance without underflowing exhausted requests.
    #[test]
    fn concurrent_branches_share_one_request_budget() {
        let cancelled = AtomicBool::new(false);
        let budget = Budget::new(&cancelled);
        let reserve = || {
            (0..MAX_REQUESTS)
                .map(|_| budget.request())
                .collect::<Vec<_>>()
        };
        let reservations = std::thread::scope(|scope| {
            let other = scope.spawn(reserve);
            [reserve(), other.join().unwrap()]
        });
        assert_eq!(
            reservations
                .iter()
                .flatten()
                .filter(|request| request.is_ok())
                .count(),
            MAX_REQUESTS
        );
        assert_eq!(
            reservations
                .iter()
                .flatten()
                .filter(|request| **request == Err(Failure::Budget))
                .count(),
            MAX_REQUESTS
        );
        assert_eq!(budget.remaining_requests.load(Ordering::Relaxed), 0);
    }

    /// Both branches observe the same cancellation or deadline before issuing any request.
    #[test]
    fn stopped_shared_budgets_prevent_requests_from_both_branches() {
        for is_cancelled in [true, false] {
            let cancelled = AtomicBool::new(is_cancelled);
            let mut budget = Budget::new(&cancelled);
            let expected = if is_cancelled {
                Failure::Cancelled
            } else {
                budget.deadline = Instant::now();
                Failure::Timeout
            };
            let transport = MockTransport::default();
            let website_url = Url::parse("https://artist.co.uk/").unwrap();
            let registration_url = Url::parse(BOOTSTRAP_URL).unwrap();
            std::thread::scope(|scope| {
                let website = scope.spawn(|| {
                    fetch_document(&transport, &website_url, DocumentKind::Html, &budget)
                });
                assert_eq!(
                    fetch_document(&transport, &registration_url, DocumentKind::Json, &budget)
                        .unwrap_err(),
                    expected
                );
                assert_eq!(website.join().unwrap().unwrap_err(), expected);
            });
            assert!(transport.requests.lock().unwrap().is_empty());
            assert_eq!(
                budget.remaining_requests.load(Ordering::Relaxed),
                MAX_REQUESTS
            );
        }
    }
}
