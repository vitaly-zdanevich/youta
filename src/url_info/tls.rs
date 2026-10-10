//! Certificate metadata from the same verified HTTPS connection that fetches a page.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport, TransportAdapter,
};
use x509_cert::der::Decode;
use x509_cert::ext::pkix::{SubjectAltName, name::GeneralName};

use super::Facts;

/// Maximum certificate size parsed for display; TLS verification has its own limits.
const MAX_CERTIFICATE_BYTES: usize = 64 * 1024;
/// Avoids projecting unbounded domain lists from a multi-tenant server certificate.
const MAX_CERTIFICATE_DOMAINS: usize = 64;

/// Bounded, terminal-safe information observed on a successfully verified TLS connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct TlsInfo {
    pub(super) facts: Vec<String>,
}

/// Wraps an existing TCP connection in standard verified TLS and records peer metadata.
#[derive(Debug)]
pub(super) struct InfoTlsConnector {
    captured: Arc<Mutex<Option<TlsInfo>>>,
}

impl InfoTlsConnector {
    /// Shares one result slot with the website request; no additional connection is made.
    pub(super) fn new(captured: Arc<Mutex<Option<TlsInfo>>>) -> Self {
        Self { captured }
    }
}

impl<In: Transport> Connector<In> for InfoTlsConnector {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            return Ok(None);
        };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(transport.boxed()));
        }
        let host = details.uri.host().ok_or(ureq::Error::ConnectionFailed)?;
        // URI authorities enclose IPv6 literals in brackets; rustls expects the address.
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        let server_name = ServerName::try_from(host.to_owned())
            .map_err(|_| ureq::Error::Tls("Invalid TLS server name"))?;
        let connection = ClientConnection::new(default_client_config(), server_name)?;
        Ok(Some(Box::new(InfoTlsTransport {
            stream: StreamOwned::new(connection, DeadlineIo::new(transport.boxed())),
            buffers: LazyBuffers::new(
                details.config.input_buffer_size(),
                details.config.output_buffer_size(),
            ),
            captured: self.captured.clone(),
            recorded: false,
        })))
    }
}

/// Uses the same provider selection as ureq's default rustls connector.
fn crypto_provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// Shares the normal WebPKI root store and verified client configuration across requests.
fn default_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            Arc::new(
                ClientConfig::builder_with_provider(crypto_provider())
                    .with_safe_default_protocol_versions()
                    .expect("default TLS versions supported by the crypto provider")
                    .with_root_certificates(RootCertStore {
                        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                    })
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// Adapts rustls to ureq while retaining access to its verified connection state.
struct InfoTlsTransport {
    stream: StreamOwned<ClientConnection, DeadlineIo>,
    buffers: LazyBuffers,
    captured: Arc<Mutex<Option<TlsInfo>>>,
    recorded: bool,
}

/// Shrinks the remaining budget for each TLS-internal socket read/write, not each handshake.
///
/// Rustls can perform several underlying operations inside a single ureq transport call.
/// Reusing an unchanged relative timeout for each would let fragmented handshakes exceed
/// the caller's deadline. Each new ureq operation supplies its reduced global budget.
struct DeadlineIo {
    inner: TransportAdapter,
    started: Instant,
    timeout: NextTimeout,
}

impl DeadlineIo {
    /// Adapts an already connected transport without changing its certificate or DNS policy.
    fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            inner: TransportAdapter::new(transport),
            started: Instant::now(),
            timeout: NextTimeout {
                after: ureq::unversioned::transport::time::Duration::NotHappening,
                reason: ureq::Timeout::Global,
            },
        }
    }

    /// Starts one transport operation using the remaining deadline supplied by ureq.
    fn set_timeout(&mut self, timeout: NextTimeout) {
        self.started = Instant::now();
        self.timeout = timeout;
    }

    /// Computes the diminishing budget without resetting it after an internal TLS operation.
    fn remaining_timeout_at(&self, now: Instant) -> Result<NextTimeout, ureq::Error> {
        use ureq::unversioned::transport::time::Duration;
        let after = match self.timeout.after {
            Duration::NotHappening => Duration::NotHappening,
            Duration::Exact(duration) => {
                let remaining =
                    duration.saturating_sub(now.saturating_duration_since(self.started));
                if remaining.is_zero() {
                    return Err(ureq::Error::Timeout(self.timeout.reason));
                }
                Duration::Exact(remaining)
            }
        };
        Ok(NextTimeout {
            after,
            reason: self.timeout.reason,
        })
    }

    /// Applies the deadline immediately before the underlying socket operation.
    fn prepare_io(&mut self) -> std::io::Result<()> {
        let timeout = self
            .remaining_timeout_at(Instant::now())
            .map_err(ureq::Error::into_io)?;
        self.inner.set_timeout(timeout);
        Ok(())
    }

    /// Delegates ureq's connection-liveness check to the original TCP transport.
    fn get_mut(&mut self) -> &mut dyn Transport {
        self.inner.get_mut()
    }
}

impl Read for DeadlineIo {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.prepare_io()?;
        self.inner.read(bytes)
    }
}

impl Write for DeadlineIo {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.prepare_io()?;
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.prepare_io()?;
        self.inner.flush()
    }
}

impl std::fmt::Debug for InfoTlsTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InfoTlsTransport")
            .finish_non_exhaustive()
    }
}

impl InfoTlsTransport {
    /// Records facts only after the normal certificate and hostname checks succeeded.
    fn record_handshake(&mut self) {
        if self.recorded || self.stream.conn.is_handshaking() {
            return;
        }
        self.recorded = true;
        let mut facts = Facts::default();
        match self.stream.conn.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => facts.add("TLS version", "1.3"),
            Some(rustls::ProtocolVersion::TLSv1_2) => facts.add("TLS version", "1.2"),
            _ => {}
        }
        if let Some(certificate) = self
            .stream
            .conn
            .peer_certificates()
            .and_then(|chain| chain.first())
        {
            facts.extend(certificate_facts(certificate.as_ref()));
        }
        if let Ok(mut captured) = self.captured.lock() {
            *captured = Some(TlsInfo {
                facts: facts.finish(),
            });
        }
    }
}

impl Transport for InfoTlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        self.stream.write_all(&self.buffers.output()[..amount])?;
        self.record_handshake();
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let amount = self.stream.read(self.buffers.input_append_buf())?;
        self.buffers.input_appended(amount);
        self.record_handshake();
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}

/// Parses a bounded leaf certificate for display only, never for trust decisions.
fn certificate_facts(der: &[u8]) -> Vec<String> {
    if der.len() > MAX_CERTIFICATE_BYTES {
        return Vec::new();
    }
    let Ok(certificate) = x509_cert::Certificate::from_der(der) else {
        return Vec::new();
    };
    let certificate = certificate.tbs_certificate;
    let mut facts = Facts::default();
    facts.add("Certificate issuer", &certificate.issuer.to_string());
    let expires = certificate.validity.not_after.to_date_time();
    facts.add(
        "Certificate expires",
        &format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
            expires.year(),
            expires.month(),
            expires.day(),
            expires.hour(),
            expires.minutes(),
            expires.seconds(),
        ),
    );
    if let Ok(Some((_, alternative_names))) = certificate.get::<SubjectAltName>() {
        let names = alternative_names
            .0
            .iter()
            .filter_map(|name| match name {
                GeneralName::DnsName(name) => Some(name.as_str()),
                _ => None,
            })
            .take(MAX_CERTIFICATE_DOMAINS)
            .collect::<Vec<_>>();
        facts.add("Certificate domains", &names.join(", "));
    }
    facts.finish()
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
