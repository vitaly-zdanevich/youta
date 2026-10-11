//! Scripted HTTP regressions for the production LibriVox transport.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

type HttpReply = Result<ureq::http::Response<ureq::Body>, ureq::Error>;
type RequestLog = Arc<Mutex<Vec<String>>>;

/// Supplies HTTP responses at the agent boundary without DNS or real sockets.
fn scripted_transport(
    replies: impl IntoIterator<Item = HttpReply>,
    timeout: Duration,
) -> (UreqLibrivoxTransport, RequestLog) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&requests);
    let responses = Mutex::new(replies.into_iter().collect::<VecDeque<_>>());
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .middleware(
            move |request: ureq::http::Request<ureq::SendBody>,
                  _next: ureq::middleware::MiddlewareNext| {
                assert_eq!(request.method(), "GET");
                assert_eq!(request.headers()["accept"], "application/json");
                observed
                    .lock()
                    .expect("request log")
                    .push(request.uri().to_string());
                responses
                    .lock()
                    .expect("response queue")
                    .pop_front()
                    .expect("unexpected HTTP request")
            },
        )
        .build()
        .into();
    (UreqLibrivoxTransport { agent }, requests)
}

/// Builds a complete bounded response for one scripted attempt.
fn response(status: u16, body: &[u8]) -> HttpReply {
    Ok(ureq::http::Response::builder()
        .status(status)
        .body(ureq::Body::builder().data(body.to_vec()))
        .expect("scripted response"))
}

/// Advances a fake monotonic clock instead of sleeping during retries.
fn fetch(transport: &UreqLibrivoxTransport, max_bytes: usize) -> Result<Vec<u8>, ProviderError> {
    let elapsed = Cell::new(Duration::ZERO);
    transport.fetch_with_timing(
        &Url::parse(AUDIOBOOKS_ENDPOINT).unwrap(),
        "application/json",
        max_bytes,
        || elapsed.get(),
        |delay| elapsed.set(elapsed.get() + delay),
    )
}

/// A temporary server failure must not hide the following successful response.
#[test]
fn transient_http_error_retries_the_same_validated_get() {
    let (transport, requests) = scripted_transport(
        [response(500, b"outage"), response(200, b"{\"books\":[]}")],
        DEFAULT_REQUEST_TIMEOUT,
    );
    let url = Url::parse(AUDIOBOOKS_ENDPOINT).expect("valid catalogue URL");
    let bytes = transport
        .fetch(&url, "application/json", 1024)
        .expect("temporary HTTP failure must recover");
    assert_eq!(bytes, b"{\"books\":[]}");
    assert_eq!(*requests.lock().unwrap(), vec![url.to_string(); 2]);
}

/// Each explicitly transient status may recover; persistent failures stop after three attempts.
#[test]
fn transient_statuses_recover_and_persistent_failure_returns_last_status() {
    for status in [500, 502, 503, 504] {
        let (transport, requests) = scripted_transport(
            [response(status, b"outage"), response(200, b"ok")],
            DEFAULT_REQUEST_TIMEOUT,
        );
        assert_eq!(fetch(&transport, 1024).unwrap(), b"ok");
        assert_eq!(requests.lock().unwrap().len(), 2);
    }
    let (transport, requests) = scripted_transport(
        [500, 502, 504].map(|status| response(status, b"outage")),
        DEFAULT_REQUEST_TIMEOUT,
    );
    assert!(matches!(
        fetch(&transport, 1024),
        Err(ProviderError::HttpStatus(504))
    ));
    assert_eq!(requests.lock().unwrap().len(), 3);
}

/// Permanent errors and server retry hints return immediately without replaying GET.
#[test]
fn permanent_statuses_and_retry_after_never_retry() {
    for status in [400, 404, 429, 501] {
        let (transport, requests) =
            scripted_transport([response(status, b"error")], DEFAULT_REQUEST_TIMEOUT);
        assert!(
            matches!(fetch(&transport, 1024), Err(ProviderError::HttpStatus(code)) if code == status)
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
    for hint in [
        "0",
        "1",
        "3600",
        "Wed, 21 Oct 2037 07:28:00 GMT",
        "invalid",
        "",
    ] {
        let mut reply = response(503, b"outage").unwrap();
        reply
            .headers_mut()
            .insert("retry-after", hint.parse().unwrap());
        let (transport, requests) = scripted_transport([Ok(reply)], DEFAULT_REQUEST_TIMEOUT);
        assert!(matches!(
            fetch(&transport, 1024),
            Err(ProviderError::HttpStatus(503))
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}

/// A redirect cannot become a retry or change the fixed validated destination.
#[test]
fn redirects_and_invalid_origins_never_start_another_request() {
    let mut reply = response(302, b"").unwrap();
    reply
        .headers_mut()
        .insert("location", "https://example.com/".parse().unwrap());
    let (transport, requests) = scripted_transport([Ok(reply)], DEFAULT_REQUEST_TIMEOUT);
    assert!(matches!(
        fetch(&transport, 1024),
        Err(ProviderError::HttpStatus(302))
    ));
    assert_eq!(requests.lock().unwrap().len(), 1);
    for url in [
        "http://librivox.org/",
        "https://example.com/",
        "https://user@librivox.org/",
        "https://librivox.org/#fragment",
    ] {
        assert!(matches!(
            transport.fetch(&Url::parse(url).unwrap(), "application/json", 1024),
            Err(ProviderError::InvalidRequest(_))
        ));
    }
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// Response limits still apply to both known-length and streamed successful retry bodies.
#[test]
fn oversized_retry_bodies_are_never_retried_again() {
    for body in [
        ureq::Body::builder().data(b"large".to_vec()),
        ureq::Body::builder().reader(Cursor::new(b"large")),
    ] {
        let reply = ureq::http::Response::builder()
            .status(200)
            .body(body)
            .unwrap();
        let (transport, requests) = scripted_transport(
            [response(503, b"outage"), Ok(reply)],
            DEFAULT_REQUEST_TIMEOUT,
        );
        assert!(matches!(
            fetch(&transport, 2),
            Err(ProviderError::ResponseTooLarge { limit: 2 })
        ));
        assert_eq!(requests.lock().unwrap().len(), 2);
    }
}

/// DNS, socket, timeout and parser errors do not qualify as transient HTTP statuses.
#[test]
fn transport_and_json_errors_are_not_replayed() {
    for error in [
        ureq::Error::HostNotFound,
        ureq::Error::Timeout(ureq::Timeout::Global),
        std::io::Error::from(std::io::ErrorKind::ConnectionReset).into(),
    ] {
        let (transport, requests) = scripted_transport([Err(error)], DEFAULT_REQUEST_TIMEOUT);
        assert!(matches!(
            fetch(&transport, 1024),
            Err(ProviderError::Transport(_))
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
    let (transport, requests) =
        scripted_transport([response(200, b"not JSON")], DEFAULT_REQUEST_TIMEOUT);
    let client = LibrivoxClient::with_transport(Arc::new(transport), 1024, 1024).unwrap();
    assert!(matches!(
        client.search(&LibrivoxSearchRequest::for_text("book", 20, 0)),
        Err(ProviderError::InvalidResponse(_))
    ));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// A retry delay must fit the original budget, including a late wakeup.
#[test]
fn exhausted_budget_prevents_another_get() {
    let (transport, requests) =
        scripted_transport([response(503, b"outage")], Duration::from_millis(400));
    assert!(fetch(&transport, 1024).is_err());
    assert_eq!(requests.lock().unwrap().len(), 1);
    let (transport, requests) = scripted_transport(
        [response(503, b"outage"), response(502, b"outage")],
        DEFAULT_REQUEST_TIMEOUT,
    );
    let elapsed = Cell::new(Duration::ZERO);
    let mut waits = Vec::new();
    assert!(
        transport
            .fetch_with_timing(
                &Url::parse(AUDIOBOOKS_ENDPOINT).unwrap(),
                "application/json",
                1024,
                || elapsed.get(),
                |delay| {
                    waits.push(delay);
                    elapsed.set(if waits.len() == 1 {
                        delay
                    } else {
                        DEFAULT_REQUEST_TIMEOUT
                    });
                },
            )
            .is_err()
    );
    assert_eq!(waits, [Duration::from_millis(500), Duration::from_secs(1)]);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

/// The elapsed budget rejects exact exhaustion without overflowing large caller limits.
#[test]
fn remaining_timeout_handles_exact_exhaustion_and_maximum_duration() {
    assert_eq!(
        remaining_request_timeout(Duration::MAX, Duration::ZERO).unwrap(),
        Duration::MAX
    );
    assert_eq!(
        remaining_request_timeout(Duration::MAX, Duration::from_secs(1)).unwrap(),
        Duration::MAX - Duration::from_secs(1)
    );
    for elapsed in [
        DEFAULT_REQUEST_TIMEOUT,
        DEFAULT_REQUEST_TIMEOUT + Duration::from_nanos(1),
    ] {
        assert!(matches!(
            remaining_request_timeout(DEFAULT_REQUEST_TIMEOUT, elapsed),
            Err(ProviderError::Transport(_))
        ));
    }
}

/// Simulates a response body finishing exactly when its total timeout expires.
struct ExpiringBody {
    bytes: Cursor<&'static [u8]>,
    elapsed: Arc<AtomicU64>,
}

impl Read for ExpiringBody {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.elapsed
            .store(DEFAULT_REQUEST_TIMEOUT.as_secs(), Ordering::Relaxed);
        self.bytes.read(buffer)
    }
}

/// Buffered body completion cannot reset or bypass the whole-fetch deadline.
#[test]
fn body_read_stays_inside_original_deadline() {
    let elapsed = Arc::new(AtomicU64::new(0));
    let body = ureq::Body::builder().reader(ExpiringBody {
        bytes: Cursor::new(b"ok"),
        elapsed: Arc::clone(&elapsed),
    });
    let reply = ureq::http::Response::builder()
        .status(200)
        .body(body)
        .unwrap();
    let (transport, requests) = scripted_transport([Ok(reply)], DEFAULT_REQUEST_TIMEOUT);
    let result = transport.fetch_with_timing(
        &Url::parse(AUDIOBOOKS_ENDPOINT).unwrap(),
        "application/json",
        1024,
        || Duration::from_secs(elapsed.load(Ordering::Relaxed)),
        |_| panic!("successful body must not retry"),
    );
    assert!(matches!(result, Err(ProviderError::Transport(_))));
    assert_eq!(requests.lock().unwrap().len(), 1);
}
