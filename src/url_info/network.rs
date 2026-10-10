//! Bounded network registration facts and reported CDN hints for the connected peer.
//!
//! Country codes describe registry data, not server geolocation. RDAP entities are
//! read only when embedded in the response; their links never cause extra requests.
//! Bootstrap routing follows <https://www.rfc-editor.org/rfc/rfc9224.html> and
//! network objects follow <https://www.rfc-editor.org/rfc/rfc9083.html#section-5.4>.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::unversioned::transport::{
    ConnectionDetails, Connector, DefaultConnector, NextTimeout, time::Instant as TransportInstant,
};
use url::Url;

use super::{
    Budget, DocumentKind, Facts, Failure, HttpTransport, LOOKUP_TIMEOUT, check_status,
    fetch_document, ip_address_is_non_public, parse_json, safe_text, validate_public_url,
};

const IPV4_BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/ipv4.json";
const IPV6_BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/ipv6.json";
const MAX_BOOTSTRAP_PREFIXES: usize = 4_096;

/// Caches validated IANA bootstrap maps, separately bounded for each address family.
#[derive(Default)]
pub(super) struct NetworkClient {
    ipv4: Option<Vec<BootstrapService>>,
    ipv6: Option<Vec<BootstrapService>>,
}

impl NetworkClient {
    /// Queries registration for the actual connected address within the parent lookup budget.
    pub(super) fn lookup(
        &mut self,
        ip: IpAddr,
        transport: &impl HttpTransport,
        budget: &Budget<'_>,
    ) -> Result<Vec<String>, Failure> {
        if ip_address_is_non_public(ip) {
            return Err(Failure::InvalidUrl);
        }
        let (cache, bootstrap_url) = if ip.is_ipv4() {
            (&mut self.ipv4, IPV4_BOOTSTRAP_URL)
        } else {
            (&mut self.ipv6, IPV6_BOOTSTRAP_URL)
        };
        if cache.is_none() {
            let source = Url::parse(bootstrap_url).map_err(|_| Failure::InvalidDocument)?;
            let document = fetch_document(transport, &source, DocumentKind::Json, budget)?;
            check_status(document.response.status)?;
            *cache = Some(parse_bootstrap(
                &parse_json(&document.response.body)?,
                ip.is_ipv4(),
            )?);
        }
        let service = cache
            .as_ref()
            .and_then(|services| matching_service(services, ip))
            .ok_or(Failure::Unavailable)?;
        let mut source = service.endpoint.clone();
        source
            .path_segments_mut()
            .map_err(|_| Failure::InvalidDocument)?
            .pop_if_empty()
            .push("ip")
            .push(&ip.to_string());
        let document = fetch_document(transport, &source, DocumentKind::Json, budget)?;
        check_status(document.response.status)?;
        let mut facts = Facts::default();
        facts.extend(network_facts(&parse_json(&document.response.body)?, ip)?);
        facts.add("Network source", document.url.as_str());
        Ok(facts.finish())
    }
}

/// One canonical binary CIDR and its validated HTTPS RDAP base URL.
#[derive(Debug)]
struct BootstrapService {
    network: IpAddr,
    prefix: u8,
    endpoint: Url,
}

/// Selects the most-specific same-family prefix as specified by RFC 9224.
fn matching_service(services: &[BootstrapService], ip: IpAddr) -> Option<&BootstrapService> {
    services
        .iter()
        .filter(|service| {
            service.network.is_ipv4() == ip.is_ipv4()
                && network_bits(service.network, service.prefix) == network_bits(ip, service.prefix)
        })
        .max_by_key(|service| service.prefix)
}

/// Converts only the significant prefix bits; a zero-length prefix covers its whole family.
fn network_bits(ip: IpAddr, prefix: u8) -> u128 {
    if prefix == 0 {
        return 0;
    }
    match ip {
        IpAddr::V4(ip) => u128::from(u32::from(ip) >> (32 - prefix)),
        IpAddr::V6(ip) => u128::from(ip) >> (128 - prefix),
    }
}

/// Ignores malformed entries while bounding cached prefixes and allowing only public HTTPS services.
fn parse_bootstrap(value: &Value, ipv4: bool) -> Result<Vec<BootstrapService>, Failure> {
    let services = value
        .get("services")
        .and_then(Value::as_array)
        .ok_or(Failure::InvalidDocument)?;
    if services.len() > MAX_BOOTSTRAP_PREFIXES {
        return Err(Failure::TooLarge);
    }
    let mut result = Vec::new();
    for service in services {
        let Some(pair) = service.as_array().filter(|pair| pair.len() == 2) else {
            continue;
        };
        let (Some(prefixes), Some(endpoints)) = (pair[0].as_array(), pair[1].as_array()) else {
            continue;
        };
        let endpoint = endpoints
            .iter()
            .take(8)
            .filter_map(Value::as_str)
            .filter_map(|value| Url::parse(value).ok())
            .find(|url| {
                url.scheme() == "https"
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && validate_public_url(url).is_ok()
            });
        let Some(endpoint) = endpoint else {
            continue;
        };
        for prefix in prefixes.iter().filter_map(Value::as_str) {
            let Some((address, prefix)) = prefix.split_once('/') else {
                continue;
            };
            let (Ok(network), Ok(prefix)) = (address.parse::<IpAddr>(), prefix.parse::<u8>())
            else {
                continue;
            };
            let bits = if ipv4 { 32 } else { 128 };
            if network.is_ipv4() != ipv4 || prefix > bits {
                continue;
            }
            let number = match network {
                IpAddr::V4(ip) => u128::from(u32::from(ip)),
                IpAddr::V6(ip) => u128::from(ip),
            };
            let canonical = if prefix == 0 {
                0
            } else {
                network_bits(network, prefix) << (bits - prefix)
            };
            if number != canonical {
                continue;
            }
            if result.len() == MAX_BOOTSTRAP_PREFIXES {
                return Err(Failure::TooLarge);
            }
            result.push(BootstrapService {
                network,
                prefix,
                endpoint: endpoint.clone(),
            });
        }
    }
    if result.is_empty() {
        Err(Failure::Unavailable)
    } else {
        Ok(result)
    }
}

/// Requires an RFC 9083 IP-network object whose same-family inclusive range contains the peer.
fn network_facts(value: &Value, ip: IpAddr) -> Result<Vec<String>, Failure> {
    let address = |field| {
        value
            .get(field)
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<IpAddr>().ok())
    };
    let (Some(start), Some(end)) = (address("startAddress"), address("endAddress")) else {
        return Err(Failure::Mismatch);
    };
    if value.get("objectClassName").and_then(Value::as_str) != Some("ip network")
        || value.get("ipVersion").and_then(Value::as_str)
            != Some(if ip.is_ipv4() { "v4" } else { "v6" })
        || start.is_ipv4() != ip.is_ipv4()
        || end.is_ipv4() != ip.is_ipv4()
        || start > ip
        || end < ip
    {
        return Err(Failure::Mismatch);
    }
    let mut facts = Facts::default();
    let registrant = registrant_name(value.get("entities"), 0, &mut 64);
    if let Some(name) = registrant
        .as_deref()
        .or_else(|| value.get("name").and_then(Value::as_str))
    {
        facts.add("Network", name);
    }
    if let Some(country) = value
        .get("country")
        .and_then(Value::as_str)
        .filter(|country| {
            country.len() == 2 && country.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
    {
        facts.add(
            "Network country (registered)",
            &country.to_ascii_uppercase(),
        );
    }
    Ok(facts.finish())
}

/// Examines bounded embedded registrants, preferring organization over formatted contact name.
fn registrant_name(
    entities: Option<&Value>,
    depth: usize,
    remaining: &mut usize,
) -> Option<String> {
    if depth > 4 {
        return None;
    }
    for entity in entities.and_then(Value::as_array)? {
        if *remaining == 0 {
            return None;
        }
        *remaining -= 1;
        let registrant = entity
            .get("roles")
            .and_then(Value::as_array)
            .is_some_and(|roles| {
                roles
                    .iter()
                    .take(32)
                    .any(|role| role.as_str() == Some("registrant"))
            });
        if registrant
            && let Some(properties) = entity
                .get("vcardArray")
                .and_then(Value::as_array)
                .filter(|vcard| vcard.first().and_then(Value::as_str) == Some("vcard"))
                .and_then(|vcard| vcard.get(1))
                .and_then(Value::as_array)
        {
            for field in ["org", "fn"] {
                for property in properties
                    .iter()
                    .take(64)
                    .filter_map(Value::as_array)
                    .filter(|property| property.len() == 4 && property[0].as_str() == Some(field))
                {
                    let value = property[3].as_str().map(safe_text).or_else(|| {
                        property[3].as_array().map(|parts| {
                            parts
                                .iter()
                                .take(8)
                                .filter_map(Value::as_str)
                                .map(safe_text)
                                .filter(|part| !part.is_empty())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                    });
                    if let Some(value) = value.filter(|value| !value.is_empty()) {
                        return Some(value);
                    }
                }
            }
        }
        if let Some(name) = registrant_name(entity.get("entities"), depth + 1, remaining) {
            return Some(name);
        }
    }
    None
}

/// Corroborated response headers report a CDN hint, not authenticated provider ownership.
pub(super) fn reported_cdns(headers: &ureq::http::HeaderMap) -> Vec<&'static str> {
    let header = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= 1_024)
    };
    let mut result = Vec::new();
    if header("cf-ray").is_some()
        && header("server").is_some_and(|server| server.eq_ignore_ascii_case("cloudflare"))
    {
        result.push("Cloudflare");
    }
    let cloudfront_via = headers
        .get_all("via")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter(|value| value.len() <= 1_024)
        .any(|value| {
            value.split_ascii_whitespace().any(|token| {
                token.eq_ignore_ascii_case("(CloudFront)")
                    || token
                        .trim_end_matches(',')
                        .to_ascii_lowercase()
                        .ends_with(".cloudfront.net")
            })
        });
    if cloudfront_via && (header("x-amz-cf-id").is_some() || header("x-amz-cf-pop").is_some()) {
        result.push("CloudFront");
    }
    result
}

/// Pins each default TCP/TLS connection attempt to one safe resolved address and records success.
///
/// The original URI is preserved for Host, SNI, and certificate checks. A fresh agent per
/// HTTP hop prevents pooled connections from bypassing this observation; proxies are refused.
#[derive(Debug)]
pub(super) struct PeerConnector<C = DefaultConnector> {
    inner: C,
    peer: Arc<Mutex<Option<IpAddr>>>,
    #[cfg(test)]
    pub(super) allow_loopback: bool,
}

impl PeerConnector {
    /// Shares only the successfully connected peer with the owning per-request transport.
    #[cfg(test)]
    pub(super) fn new(peer: Arc<Mutex<Option<IpAddr>>>) -> Self {
        Self::with_connector(peer, DefaultConnector::default())
    }
}

impl<C> PeerConnector<C> {
    /// Accepts a TCP/TLS connector chain without changing its certificate verification.
    pub(super) fn with_connector(peer: Arc<Mutex<Option<IpAddr>>>, inner: C) -> Self {
        Self {
            inner,
            peer,
            #[cfg(test)]
            allow_loopback: false,
        }
    }
}

impl<C: Connector<()>> Connector<()> for PeerConnector<C> {
    type Out = C::Out;

    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        chained: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        *self
            .peer
            .lock()
            .map_err(|_| ureq::Error::ConnectionFailed)? = None;
        if details.config.proxy().is_some() || chained.is_some() {
            return Err(ureq::Error::ConnectionFailed);
        }
        let addresses = details
            .addrs
            .iter()
            .copied()
            .filter(|address| {
                #[cfg(test)]
                if self.allow_loopback && address.ip().is_loopback() {
                    return true;
                }
                !ip_address_is_non_public(address.ip())
            })
            .collect::<Vec<_>>();
        let started = match details.now {
            TransportInstant::Exact(now) => now,
            _ => Instant::now(),
        };
        let total = (*details.timeout.after).min(LOOKUP_TIMEOUT);
        let mut last_error = ureq::Error::HostNotFound;
        for (index, address) in addresses.iter().enumerate() {
            let remaining = total
                .checked_sub(started.elapsed())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(ureq::Error::Timeout(details.timeout.reason))?;
            // ureq floors TCP attempts to 10 ms; do not start one after that margin.
            if remaining < Duration::from_millis(10) {
                return Err(ureq::Error::Timeout(details.timeout.reason));
            }
            let timeout = NextTimeout {
                after: (remaining / u32::try_from(addresses.len() - index).unwrap_or(u32::MAX))
                    .into(),
                reason: details.timeout.reason,
            };
            let mut addrs = details.resolver.empty();
            addrs.push(*address);
            let attempt = ConnectionDetails {
                uri: details.uri,
                addrs,
                config: details.config,
                request_level: details.request_level,
                resolver: details.resolver,
                now: (details.current_time)(),
                timeout,
                current_time: Arc::clone(&details.current_time),
                run_connector: Arc::clone(&details.run_connector),
            };
            match self.inner.connect(&attempt, None) {
                Ok(Some(transport)) => {
                    *self
                        .peer
                        .lock()
                        .map_err(|_| ureq::Error::ConnectionFailed)? = Some(address.ip());
                    return Ok(Some(transport));
                }
                Ok(None) => last_error = ureq::Error::ConnectionFailed,
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }
}

#[cfg(test)]
mod tests {
    use super::super::HttpResponse;
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use ureq::unversioned::resolver::{DefaultResolver, Resolver};

    /// Test connection details retain a hostname while selecting explicit resolved addresses.
    fn connection_details<'a>(
        uri: &'a ureq::http::Uri,
        config: &'a ureq::config::Config,
        resolver: &'a dyn Resolver,
        addresses: &[std::net::SocketAddr],
        timeout: Duration,
    ) -> ConnectionDetails<'a> {
        let mut addrs = resolver.empty();
        for address in addresses {
            addrs.push(*address);
        }
        ConnectionDetails {
            uri,
            addrs,
            config,
            request_level: false,
            resolver,
            now: TransportInstant::now(),
            timeout: NextTimeout {
                after: timeout.into(),
                reason: ureq::Timeout::Connect,
            },
            current_time: Arc::new(TransportInstant::now),
            run_connector: Arc::new(|_| Err(ureq::Error::ConnectionFailed)),
        }
    }

    /// The first candidate fails deterministically, allowing the selected peer to be checked.
    #[derive(Debug, Default)]
    struct CandidateConnector {
        attempts: Mutex<Vec<(std::net::SocketAddr, Duration)>>,
    }

    impl Connector<()> for CandidateConnector {
        type Out = ();

        fn connect(
            &self,
            details: &ConnectionDetails<'_>,
            _: Option<()>,
        ) -> Result<Option<Self::Out>, ureq::Error> {
            assert_eq!(details.uri.host(), Some("website.example"));
            assert_eq!(details.addrs.len(), 1);
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push((details.addrs[0], *details.timeout.after));
            if attempts.len() == 1 {
                Err(ureq::Error::ConnectionFailed)
            } else {
                Ok(Some(()))
            }
        }
    }

    /// Only the successful public candidate is recorded; TLS hostnames and timeout bounds survive.
    #[test]
    fn connector_records_successful_candidate_and_preserves_hostname() {
        let peer = Arc::new(Mutex::new(None));
        let connector =
            PeerConnector::with_connector(Arc::clone(&peer), CandidateConnector::default());
        let uri = "https://website.example/path".parse().unwrap();
        let config = ureq::Agent::config_builder().proxy(None).build();
        let resolver = DefaultResolver::default();
        let addresses =
            ["127.0.0.1:443", "8.8.8.8:443", "1.1.1.1:443"].map(|address| address.parse().unwrap());
        let details =
            connection_details(&uri, &config, &resolver, &addresses, Duration::from_secs(2));
        assert!(connector.connect(&details, None).unwrap().is_some());
        assert_eq!(*peer.lock().unwrap(), Some("1.1.1.1".parse().unwrap()));
        let attempts = connector.inner.attempts.lock().unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].0, addresses[1]);
        assert_eq!(attempts[1].0, addresses[2]);
        assert!(attempts[0].1 <= Duration::from_secs(1));
        assert!(attempts[1].1 <= Duration::from_secs(2));
    }

    /// Empty deadlines and proxy/private routes never reach a candidate connector or retain a peer.
    #[test]
    fn connector_rejects_expired_private_and_proxy_routes() {
        let uri = "https://website.example/".parse().unwrap();
        let resolver = DefaultResolver::default();
        for (address, timeout, proxy) in [
            ("8.8.8.8:443", Duration::ZERO, false),
            ("127.0.0.1:443", Duration::from_secs(2), false),
            ("8.8.8.8:443", Duration::from_secs(2), true),
        ] {
            let peer = Arc::new(Mutex::new(Some("1.1.1.1".parse().unwrap())));
            let connector =
                PeerConnector::with_connector(Arc::clone(&peer), CandidateConnector::default());
            let config = ureq::Agent::config_builder()
                .proxy(if proxy {
                    Some(ureq::Proxy::new("http://proxy.example:8080").unwrap())
                } else {
                    None
                })
                .build();
            let addresses = [address.parse().unwrap()];
            let details = connection_details(&uri, &config, &resolver, &addresses, timeout);
            assert!(connector.connect(&details, None).is_err());
            assert!(connector.inner.attempts.lock().unwrap().is_empty());
            assert!(peer.lock().unwrap().is_none());
        }
    }

    /// The default connector reports the actual reachable loopback socket without a second probe.
    #[test]
    fn default_connector_records_connected_loopback_peer() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = Arc::new(Mutex::new(None));
        let mut connector = PeerConnector::new(Arc::clone(&peer));
        connector.allow_loopback = true;
        let uri = "http://website.example/".parse().unwrap();
        let config = ureq::Agent::config_builder().proxy(None).build();
        let resolver = DefaultResolver::default();
        let addresses = [address];
        let details =
            connection_details(&uri, &config, &resolver, &addresses, Duration::from_secs(2));
        assert!(connector.connect(&details, None).unwrap().is_some());
        assert_eq!(*peer.lock().unwrap(), Some(address.ip()));
    }

    /// Matching ranges exercise both address families without public network calls.
    fn network_document(ip: IpAddr) -> Value {
        match ip {
            IpAddr::V4(_) => json!({
                "objectClassName": "ip network", "ipVersion": "v4",
                "startAddress": "8.8.8.0", "endAddress": "8.8.8.255",
                "name": "ROOT-NETWORK", "country": "US"
            }),
            IpAddr::V6(_) => json!({
                "objectClassName": "ip network", "ipVersion": "v6",
                "startAddress": "2606:4700::", "endAddress": "2606:4700:ffff:ffff:ffff:ffff:ffff:ffff",
                "name": "IPV6-NETWORK", "country": "us"
            }),
        }
    }

    /// Prefix selection uses network bits rather than textual prefix comparisons.
    #[test]
    fn bootstrap_chooses_longest_matching_prefix_in_each_family() {
        for (ip, prefixes, expected) in [
            ("8.8.8.8", ["0.0.0.0/0", "8.0.0.0/8", "8.8.0.0/16"], "16"),
            (
                "2606:4700::1111",
                ["::/0", "2606::/16", "2606:4700::/32"],
                "32",
            ),
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            let value = json!({"services": prefixes.map(|prefix| {
                let (_, length) = prefix.split_once('/').unwrap();
                json!([[prefix], [format!("https://registry.example/{length}/")]])
            })});
            let services = parse_bootstrap(&value, ip.is_ipv4()).unwrap();
            let service = matching_service(&services, ip).unwrap();
            assert_eq!(service.endpoint.path(), format!("/{expected}/"));
        }
        let services = parse_bootstrap(
            &json!({"services": [
                [["8.8.0.0/16"], ["https://registry.example/"]]
            ]}),
            true,
        )
        .unwrap();
        assert!(matching_service(&services, "8.80.0.1".parse().unwrap()).is_none());
        assert!(matching_service(&services, "2606:4700::1111".parse().unwrap()).is_none());
    }

    /// Bad CIDRs, cross-family entries, and unsafe registry URLs never enter the cache.
    #[test]
    fn bootstrap_rejects_malformed_prefixes_and_unsafe_endpoints() {
        for prefix in ["8.8.8.1/24", "8.8.8.0/33", "8.8.8.0", "::/0", "bad/8"] {
            assert!(
                parse_bootstrap(
                    &json!({"services": [
                        [[prefix], ["https://registry.example/"]]
                    ]}),
                    true
                )
                .is_err(),
                "{prefix}"
            );
        }
        for endpoint in [
            "http://registry.example/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://host.internal/",
            "https://user:secret@registry.example/",
            "https://registry.example:8443/",
            "https://registry.example/?secret=x",
            "https://registry.example/#fragment",
        ] {
            assert!(
                parse_bootstrap(
                    &json!({"services": [
                        [["8.0.0.0/8"], [endpoint]]
                    ]}),
                    true
                )
                .is_err(),
                "{endpoint}"
            );
        }
        assert!(parse_bootstrap(&json!({"services": "bad"}), true).is_err());
        let too_many = vec!["8.0.0.0/8"; MAX_BOOTSTRAP_PREFIXES + 1];
        assert_eq!(
            parse_bootstrap(
                &json!({"services": [
                    [too_many, ["https://registry.example/"]]
                ]}),
                true
            )
            .unwrap_err(),
            Failure::TooLarge
        );
    }

    /// Only a matching IP network can claim registration facts about the connected peer.
    #[test]
    fn network_facts_validate_object_family_range_and_version() {
        for address in ["8.8.8.8", "2606:4700::1111"] {
            let ip: IpAddr = address.parse().unwrap();
            let value = network_document(ip);
            assert!(network_facts(&value, ip).is_ok());
            for (field, bad) in [
                ("objectClassName", "domain"),
                ("ipVersion", "unknown"),
                ("startAddress", "9.0.0.0"),
                ("endAddress", "1.0.0.0"),
                ("startAddress", "not-an-ip"),
            ] {
                let mut invalid = value.clone();
                invalid[field] = json!(bad);
                assert_eq!(network_facts(&invalid, ip).unwrap_err(), Failure::Mismatch);
            }
        }
    }

    /// Embedded registrant organizations win over names; contact and address countries do not.
    #[test]
    fn network_facts_use_embedded_registrant_and_root_country_only() {
        let ip = "8.8.8.8".parse().unwrap();
        let mut value = network_document(ip);
        value["entities"] = json!([
            {"roles": ["abuse"], "vcardArray": ["vcard", [["org", {}, "text", "Abuse contact"]]]},
            {"roles": ["registrant"], "country": "DE", "vcardArray": ["vcard", [
                ["fn", {}, "text", "Registrant name"],
                ["org", {}, "text", ["Example <b>Network</b>", "Division"]]
            ]], "links": [{"href": "http://127.0.0.1/private"}]}
        ]);
        assert_eq!(
            network_facts(&value, ip).unwrap(),
            [
                "Network: Example Network; Division",
                "Network country (registered): US"
            ]
        );
        value["entities"][1]["vcardArray"][1] = json!([["fn", {}, "text", "Registrant name"]]);
        assert!(
            network_facts(&value, ip)
                .unwrap()
                .contains(&"Network: Registrant name".into())
        );
        for country in ["USA", "1A", "", " United States "] {
            value["country"] = json!(country);
            assert!(
                !network_facts(&value, ip)
                    .unwrap()
                    .iter()
                    .any(|fact| fact.starts_with("Network country"))
            );
        }
        value["entities"] = json!([]);
        assert_eq!(
            network_facts(&value, ip).unwrap(),
            ["Network: ROOT-NETWORK"]
        );
    }

    /// CDN markers require corroborating provider-specific response headers, not generic hosting.
    #[test]
    fn reported_cdn_markers_are_conservative() {
        let headers = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| {
                    (
                        ureq::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        ureq::http::HeaderValue::from_str(value).unwrap(),
                    )
                })
                .collect::<ureq::http::HeaderMap>()
        };
        for pairs in [
            vec![("server", "cloudflare")],
            vec![("cf-ray", "1234-LHR")],
            vec![("server", "AmazonS3"), ("x-amz-cf-id", "id")],
            vec![("via", "1.1 notcloudfront.net"), ("x-amz-cf-pop", "LHR50")],
            vec![("server", "Google Frontend"), ("x-cache", "HIT")],
            vec![("via", "1.1 node.cloudfront.net (CloudFront)")],
        ] {
            assert!(reported_cdns(&headers(&pairs)).is_empty(), "{pairs:?}");
        }
        assert_eq!(
            reported_cdns(&headers(&[
                ("server", "Cloudflare"),
                ("cf-ray", "1234-LHR")
            ])),
            ["Cloudflare"]
        );
        assert_eq!(
            reported_cdns(&headers(&[
                ("via", "1.1 node.cloudfront.net (CloudFront)"),
                ("x-amz-cf-id", "id")
            ])),
            ["CloudFront"]
        );
    }

    /// Only registry JSON is fetched; embedded entity links are never followed.
    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<VecDeque<(String, Value)>>,
        requests: Mutex<Vec<String>>,
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
            let (expected, value) = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request");
            assert_eq!(url.as_str(), expected);
            Ok(HttpResponse {
                status: 200,
                content_type: "application/rdap+json".into(),
                body: serde_json::to_vec(&value).unwrap(),
                ..HttpResponse::default()
            })
        }
    }

    /// A session caches only each family's validated bootstrap, not network facts.
    #[test]
    fn lookup_caches_each_family_and_keeps_attribution() {
        let transport = MockTransport::default();
        let mut client = NetworkClient::default();
        let cancelled = AtomicBool::new(false);
        for (index, address) in ["8.8.8.8", "2606:4700::1111", "8.8.8.8", "2606:4700::1111"]
            .into_iter()
            .enumerate()
        {
            let ip: IpAddr = address.parse().unwrap();
            if index < 2 {
                transport.responses.lock().unwrap().push_back((
                    if ip.is_ipv4() { IPV4_BOOTSTRAP_URL } else { IPV6_BOOTSTRAP_URL }.into(),
                    json!({"services": [[[if ip.is_ipv4() { "8.0.0.0/8" } else { "2606::/16" }], ["https://registry.example/rdap/"]]]})
                ));
            }
            let source = format!("https://registry.example/rdap/ip/{ip}");
            let mut document = network_document(ip);
            document["entities"] = json!([{
                "roles": ["registrant"],
                "links": [{"href": "http://127.0.0.1/private"}]
            }]);
            transport
                .responses
                .lock()
                .unwrap()
                .push_back((source.clone(), document));
            let facts = client
                .lookup(ip, &transport, &Budget::new(&cancelled))
                .unwrap();
            assert!(facts.contains(&format!("Network source: {source}")));
            assert!(facts.contains(&"Network country (registered): US".into()));
        }
        assert!(transport.responses.lock().unwrap().is_empty());
        assert_eq!(transport.requests.lock().unwrap().len(), 6);
        assert_eq!(
            client
                .lookup(
                    "127.0.0.1".parse().unwrap(),
                    &transport,
                    &Budget::new(&cancelled)
                )
                .unwrap_err(),
            Failure::InvalidUrl
        );
    }
}
