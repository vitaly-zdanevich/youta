//! In-memory verified TLS exchanges using a fixed, test-only certificate authority.

use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime, pem::PemObject};
use rustls::{ServerConfig, ServerConnection};
use std::collections::VecDeque;
use std::io::Cursor;
use std::time::Duration;
use x509_cert::der::{
    Decode, Encode,
    asn1::{Ia5String, OctetString},
};
use x509_cert::ext::pkix::{SubjectAltName, name::GeneralName};

const LEAF: &[u8] = include_bytes!("tls_fixtures/leaf.pem");
const ROOT: &[u8] = include_bytes!("tls_fixtures/root.pem");
const KEY: &[u8] = include_bytes!("tls_fixtures/leaf.key");

#[test]
fn certificate_projection_contains_issuer_expiry_and_dns_names() {
    let der = CertificateDer::from_pem_slice(LEAF).unwrap();
    let info = certificate_facts(der.as_ref());
    assert!(info.iter().any(|line| {
        line.starts_with("Certificate issuer: ") && line.contains("Youta URL Info Test CA")
    }));
    assert!(
        info.iter()
            .any(|line| { line == "Certificate expires: 2126-09-16 17:23:53 UTC" })
    );
    assert!(
        info.iter().any(|line| {
            line == "Certificate domains: example.test, *.example.test, other.test"
        })
    );
    assert!(info.iter().all(|line| !line.contains("owner")));
}

#[test]
fn malformed_and_oversized_certificates_produce_no_certificate_facts() {
    for bytes in [
        vec![],
        vec![0x30, 0x01, 0xff],
        vec![0; MAX_CERTIFICATE_BYTES + 1],
    ] {
        assert!(certificate_facts(&bytes).is_empty());
    }
}

#[test]
fn certificate_domains_are_bounded_and_ignore_non_dns_alternative_names() {
    let der = CertificateDer::from_pem_slice(LEAF).unwrap();
    let mut certificate = x509_cert::Certificate::from_der(der.as_ref()).unwrap();
    let mut names = vec![GeneralName::UniformResourceIdentifier(
        Ia5String::new("https://unrelated.test").unwrap(),
    )];
    names.extend(
        (0..100)
            .map(|index| GeneralName::DnsName(Ia5String::new(&format!("d{index}.test")).unwrap())),
    );
    let extension = certificate
        .tbs_certificate
        .extensions
        .as_mut()
        .unwrap()
        .iter_mut()
        .find(|extension| extension.extn_id.to_string() == "2.5.29.17")
        .unwrap();
    extension.extn_value = OctetString::new(SubjectAltName(names).to_der().unwrap()).unwrap();
    let facts = certificate_facts(&certificate.to_der().unwrap());
    let domains = facts
        .iter()
        .find(|line| line.starts_with("Certificate domains: "))
        .unwrap();
    assert!(domains.contains("d63.test"));
    assert!(!domains.contains("d64.test"));
    assert!(!domains.contains("unrelated.test"));
    assert!(domains.len() <= super::super::MAX_FIELD_BYTES);
}

#[test]
fn repeated_tls_io_shares_one_deadline_and_preserves_the_timeout_reason() {
    let captured = Arc::new(Mutex::new(None));
    let mut transport = test_transport(
        trusted_test_config(),
        "example.test",
        &rustls::version::TLS13,
        captured,
    );
    let socket = transport.stream.get_mut();
    socket.set_timeout(NextTimeout {
        after: ureq::unversioned::transport::time::Duration::from_secs(8),
        reason: ureq::Timeout::SendRequest,
    });
    let started = socket.started;
    let after_three = socket
        .remaining_timeout_at(started + Duration::from_secs(3))
        .unwrap();
    assert_eq!(*after_three.after, Duration::from_secs(5));
    let after_six = socket
        .remaining_timeout_at(started + Duration::from_secs(6))
        .unwrap();
    assert_eq!(*after_six.after, Duration::from_secs(2));
    assert_eq!(after_six.reason, ureq::Timeout::SendRequest);
    assert!(matches!(
        socket.remaining_timeout_at(started + Duration::from_secs(8)),
        Err(ureq::Error::Timeout(ureq::Timeout::SendRequest))
    ));
}

#[test]
fn tls13_metadata_comes_from_the_connection_that_serves_the_page() {
    assert_verified_exchange(&rustls::version::TLS13, "TLS version: 1.3");
}

#[test]
fn tls12_metadata_comes_from_the_connection_that_serves_the_page() {
    assert_verified_exchange(&rustls::version::TLS12, "TLS version: 1.2");
}

#[test]
fn default_trust_store_rejects_the_untrusted_test_certificate() {
    let captured = Arc::new(Mutex::new(None));
    let mut config = (*default_client_config()).clone();
    config.time_provider = Arc::new(TestTime);
    let mut transport = test_transport(
        Arc::new(config),
        "example.test",
        &rustls::version::TLS13,
        captured.clone(),
    );
    let error = send_request(&mut transport).unwrap_err();
    assert!(error.to_string().contains("UnknownIssuer"), "{error}");
    assert!(captured.lock().unwrap().is_none());
}

#[test]
fn trusted_certificate_with_wrong_hostname_is_rejected() {
    let captured = Arc::new(Mutex::new(None));
    let mut transport = test_transport(
        trusted_test_config(),
        "wrong.test",
        &rustls::version::TLS13,
        captured.clone(),
    );
    let error = send_request(&mut transport).unwrap_err();
    assert!(error.to_string().contains("not valid for name"), "{error}");
    assert!(captured.lock().unwrap().is_none());
}

/// Exchanges real TLS records entirely in memory, without adding another handshake.
fn assert_verified_exchange(version: &'static rustls::SupportedProtocolVersion, expected: &str) {
    let captured = Arc::new(Mutex::new(None));
    let mut transport = test_transport(
        trusted_test_config(),
        "example.test",
        version,
        captured.clone(),
    );
    assert!(captured.lock().unwrap().is_none());
    send_request(&mut transport).unwrap();
    assert!(transport.await_input(test_timeout()).unwrap());
    assert!(transport.buffers().input().ends_with(b"page"));
    let info = captured.lock().unwrap().clone().unwrap();
    assert_eq!(info.facts[0], expected);
    assert!(
        info.facts
            .iter()
            .any(|line| line.starts_with("Certificate issuer: "))
    );
    assert!(
        info.facts
            .iter()
            .any(|line| line.starts_with("Certificate expires: "))
    );
    assert!(
        info.facts
            .iter()
            .any(|line| line.starts_with("Certificate domains: "))
    );
    assert!(
        info.facts
            .iter()
            .all(|line| line.len() <= super::super::MAX_FIELD_BYTES)
    );
    assert!(info.facts.iter().map(String::len).sum::<usize>() <= super::super::MAX_FACT_BYTES);
}

/// Sends a complete HTTP request through the same TLS transport inspected by the client.
fn send_request(transport: &mut InfoTlsTransport) -> Result<(), ureq::Error> {
    let request = b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n";
    transport.buffers().output()[..request.len()].copy_from_slice(request);
    transport.transmit_output(request.len(), test_timeout())
}

/// Supplies a finite timeout through the production transport adapter.
fn test_timeout() -> NextTimeout {
    NextTimeout {
        after: ureq::unversioned::transport::time::Duration::from_secs(1),
        reason: ureq::Timeout::Global,
    }
}

/// A stable validation time keeps the fixture independent of the test machine's clock.
#[derive(Debug)]
struct TestTime;

impl rustls::time_provider::TimeProvider for TestTime {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(
            1_800_000_000,
        )))
    }
}

/// Only tests can replace the production WebPKI roots with the local fixture CA.
fn trusted_test_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(ROOT).unwrap())
        .unwrap();
    let mut config = ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.time_provider = Arc::new(TestTime);
    Arc::new(config)
}

/// Builds a production TLS transport over an in-memory TLS server connection.
fn test_transport(
    config: Arc<ClientConfig>,
    host: &'static str,
    version: &'static rustls::SupportedProtocolVersion,
    captured: Arc<Mutex<Option<TlsInfo>>>,
) -> InfoTlsTransport {
    let connection = ClientConnection::new(config, host.try_into().unwrap()).unwrap();
    let server_config = ServerConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(&[version])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(LEAF).unwrap()],
            PrivateKeyDer::from_pem_slice(KEY).unwrap(),
        )
        .unwrap();
    let server = MemoryServer {
        connection: ServerConnection::new(Arc::new(server_config)).unwrap(),
        buffers: LazyBuffers::new(16_384, 16_384),
        pending: VecDeque::new(),
        responded: false,
    };
    InfoTlsTransport {
        stream: StreamOwned::new(connection, DeadlineIo::new(server.boxed())),
        buffers: LazyBuffers::new(16_384, 16_384),
        captured,
        recorded: false,
    }
}

/// Minimal server-side transport: it consumes client TLS records and returns encrypted HTTP.
#[derive(Debug)]
struct MemoryServer {
    connection: ServerConnection,
    buffers: LazyBuffers,
    pending: VecDeque<u8>,
    responded: bool,
}

impl Transport for MemoryServer {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, _: NextTimeout) -> Result<(), ureq::Error> {
        let output = self.buffers.output()[..amount].to_vec();
        self.connection.read_tls(&mut Cursor::new(output))?;
        self.connection.process_new_packets()?;
        if !self.responded {
            let mut request = [0; 1024];
            if self
                .connection
                .reader()
                .read(&mut request)
                .is_ok_and(|count| count > 0)
            {
                self.connection.writer().write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npage",
                )?;
                self.responded = true;
            }
        }
        while self.connection.wants_write() {
            let mut records = Vec::new();
            self.connection.write_tls(&mut records)?;
            self.pending.extend(records);
        }
        Ok(())
    }

    fn await_input(&mut self, _: NextTimeout) -> Result<bool, ureq::Error> {
        let target = self.buffers.input_append_buf();
        let count = target.len().min(self.pending.len());
        for byte in &mut target[..count] {
            *byte = self.pending.pop_front().unwrap();
        }
        self.buffers.input_appended(count);
        Ok(count > 0)
    }

    fn is_open(&mut self) -> bool {
        true
    }
}
