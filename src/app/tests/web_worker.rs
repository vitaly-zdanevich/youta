//! Bounded localhost integration tests for the controller's real Web worker.

use super::*;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};

const WORKER_TEST_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_TEST_POLL: Duration = Duration::from_millis(2);

/// Owns a finite local HTTP fixture whose gate and accept loop also stop on panic.
struct WorkerHttpFixture {
    base: url::Url,
    requested: Receiver<String>,
    release_first: Sender<()>,
    stop: Sender<()>,
    thread: Option<JoinHandle<Result<Vec<String>, String>>>,
}

impl WorkerHttpFixture {
    /// Starts an isolated server, optionally holding `/a/` until explicitly released.
    fn new(block_first: bool) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("fixture listener");
        let address = listener.local_addr().expect("fixture listening address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking fixture accept");
        let (stop, stopped) = bounded(1);
        let (release_first, released) = bounded(1);
        let (requests, requested) = bounded(16);
        let thread = thread::spawn(move || {
            serve_worker_fixture(&listener, &stopped, &released, &requests, block_first)
        });
        Self {
            base: url::Url::parse(&format!("http://{address}/")).expect("fixture base URL"),
            requested,
            release_first,
            stop,
            thread: Some(thread),
        }
    }

    /// Resolves a fixture location without hard-coding its ephemeral TCP port.
    fn url(&self, path: &str) -> url::Url {
        self.base.join(path).expect("fixture relative URL")
    }

    /// Stops and reaps the fixture, returning all requests for exact assertions.
    fn finish(mut self) -> Vec<String> {
        self.signal_stop();
        self.thread
            .take()
            .expect("fixture thread")
            .join()
            .expect("fixture thread did not panic")
            .expect("fixture HTTP exchange succeeded")
    }

    /// Uses nonblocking notifications so cleanup cannot hang behind a full channel.
    fn signal_stop(&self) {
        let _ = self.stop.try_send(());
        let _ = self.release_first.try_send(());
    }
}

impl Drop for WorkerHttpFixture {
    fn drop(&mut self) {
        self.signal_stop();
        if let Some(thread) = self.thread.take() {
            // Accept, request reads, response writes, and the gate are all bounded.
            // Suppress fixture errors during unwinding to preserve the test failure.
            let _ = thread.join();
        }
    }
}

/// Serves only fixture HTML; no test route opens a media file or another server.
fn serve_worker_fixture(
    listener: &TcpListener,
    stopped: &Receiver<()>,
    released: &Receiver<()>,
    requests: &Sender<String>,
    block_first: bool,
) -> Result<Vec<String>, String> {
    let mut seen = Vec::new();
    let mut deadline = Instant::now() + WORKER_TEST_TIMEOUT;
    loop {
        if stopped.try_recv().is_ok() {
            return Ok(seen);
        }
        if Instant::now() >= deadline {
            return Err("fixture HTTP accept timed out".to_owned());
        }
        let (mut socket, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(WORKER_TEST_POLL);
                continue;
            }
            Err(error) => return Err(format!("fixture accept failed: {error}")),
        };
        socket
            .set_nonblocking(false)
            .map_err(|error| error.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|error| error.to_string())?;
        socket
            .set_write_timeout(Some(Duration::from_millis(100)))
            .map_err(|error| error.to_string())?;
        let Some(path) = read_worker_request(&mut socket, stopped)? else {
            return Ok(seen);
        };
        seen.push(path.clone());
        requests
            .try_send(path.clone())
            .map_err(|_| "too many fixture requests".to_owned())?;
        if block_first && seen.len() == 1 && path == "/a/" {
            crossbeam_channel::select! {
                recv(stopped) -> _ => return Ok(seen),
                recv(released) -> _ => {},
                default(WORKER_TEST_TIMEOUT) => return Err("fixture response gate timed out".to_owned()),
            }
        }
        let (status, html) = match path.as_str() {
            "/library/" => (
                "200 OK",
                "<a href='a-first/'>First</a><a href='album/'>Album</a><a href='00.opus'>Track</a>",
            ),
            "/library/album/" => (
                "200 OK",
                "<a href='../'>Parent</a><a href='01%20song.opus?token=worker-fixture-secret'>Song</a>",
            ),
            "/a/" => ("200 OK", "<a href='stale-a.opus'>Stale A</a>"),
            "/b/" => ("200 OK", "<a href='skipped-b.opus'>Skipped B</a>"),
            "/c/" => ("200 OK", "<a href='latest-c.opus'>Latest C</a>"),
            "/empty/" => ("200 OK", "<p>No playable links</p>"),
            _ => ("404 Not Found", "<p>Missing fixture</p>"),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len(),
        );
        socket
            .write_all(response.as_bytes())
            .map_err(|error| error.to_string())?;
        deadline = Instant::now() + WORKER_TEST_TIMEOUT;
    }
}

/// Reads one bounded request and checks the stop signal between short socket waits.
fn read_worker_request(
    socket: &mut TcpStream,
    stopped: &Receiver<()>,
) -> Result<Option<String>, String> {
    let deadline = Instant::now() + WORKER_TEST_TIMEOUT;
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        if stopped.try_recv().is_ok() {
            return Ok(None);
        }
        if Instant::now() >= deadline || request.len() >= 4096 {
            return Err("fixture request exceeded its time or size bound".to_owned());
        }
        let mut byte = [0_u8; 1];
        match socket.read(&mut byte) {
            Ok(0) => return Err("fixture request closed before its headers".to_owned()),
            Ok(_) => request.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("fixture request read failed: {error}")),
        }
    }
    let request = String::from_utf8(request).map_err(|error| error.to_string())?;
    request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|path| Some(path.to_owned()))
        .ok_or_else(|| "fixture request has no HTTP target".to_owned())
}

/// Drives the actual controller loop with a deadline and optional stale-row assertions.
fn wait_for_web_worker(controller: &mut AppController, mut check: impl FnMut(&AppController)) {
    let deadline = Instant::now() + WORKER_TEST_TIMEOUT;
    loop {
        controller.tick();
        check(controller);
        if !controller.web.pending && controller.web.worker.is_none() {
            return;
        }
        assert!(Instant::now() < deadline, "controller Web worker timed out");
        thread::sleep(WORKER_TEST_POLL);
    }
}

#[test]
fn web_worker_navigates_http_folders_restores_parent_selection_and_plays_exact_media() {
    let (mut controller, played) = controller_with_mock_statuses([]);
    // Declared after the controller so panic cleanup closes HTTP before controller drop.
    let server = WorkerHttpFixture::new(false);
    controller.show_screen(Screen::Web);
    controller.view.search_query = server.url("/library/").to_string();
    controller.submit_web_url();
    wait_for_web_worker(&mut controller, |_| {});
    assert_eq!(
        controller
            .view
            .rows
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        ["..", "a-first/", "album/", "00.opus"]
    );
    controller.select_row(2);
    controller.activate_selection();
    wait_for_web_worker(&mut controller, |_| {});
    assert_eq!(
        controller.web.listing.as_ref().expect("child listing").url,
        server.url("/library/album/")
    );
    assert_eq!(controller.view.rows[0].title, "..");
    controller.select_row(1);
    controller.activate_selection();
    {
        let played = played.lock().expect("mock playback state");
        assert_eq!(played.played.len(), 1);
        assert_eq!(
            played.played[0].location,
            server
                .url("/library/album/01%20song.opus?token=worker-fixture-secret")
                .as_str()
        );
        assert!(played.played[0].bypass_ytdl);
    }
    controller.select_row(0);
    controller.activate_selection();
    wait_for_web_worker(&mut controller, |_| {});
    assert_eq!(
        controller
            .web
            .listing
            .as_ref()
            .expect("restored listing")
            .url,
        server.url("/library/")
    );
    assert_eq!(controller.view.selected, 2);
    assert_eq!(
        controller.view.rows[controller.view.selected].title,
        "album/"
    );
    assert_eq!(
        server.finish(),
        ["/library/", "/library/album/", "/library/"]
    );
}

#[test]
fn web_worker_coalesces_pending_navigation_and_never_displays_the_stale_response() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    let server = WorkerHttpFixture::new(true);
    controller.show_screen(Screen::Web);
    controller.view.search_query = server.url("/a/").to_string();
    controller.submit_web_url();
    assert_eq!(
        server
            .requested
            .recv_timeout(WORKER_TEST_TIMEOUT)
            .expect("first request reached its gate"),
        "/a/"
    );
    for path in ["/b/", "/c/"] {
        controller.view.search_query = server.url(path).to_string();
        controller.submit_web_url();
    }
    assert!(controller.web.pending);
    assert!(controller.view.rows.is_empty());
    server
        .release_first
        .try_send(())
        .expect("release first request");
    wait_for_web_worker(&mut controller, |controller| {
        assert!(
            controller
                .view
                .rows
                .iter()
                .all(|row| !matches!(row.title.as_str(), "stale-a.opus" | "skipped-b.opus"))
        );
        if let Some(listing) = &controller.web.listing {
            assert_eq!(listing.url.path(), "/c/");
        }
    });
    assert_eq!(
        controller.web.listing.as_ref().expect("latest listing").url,
        server.url("/c/")
    );
    assert_eq!(controller.view.rows[1].title, "latest-c.opus");
    assert_eq!(controller.view.search_query, server.url("/c/").as_str());
    assert_eq!(server.finish(), ["/a/", "/c/"]);
}

#[test]
fn web_worker_distinguishes_an_empty_http_listing_from_an_http_failure() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    let server = WorkerHttpFixture::new(false);
    controller.show_screen(Screen::Web);
    controller.view.search_query = server.url("/empty/").to_string();
    controller.submit_web_url();
    wait_for_web_worker(&mut controller, |_| {});
    assert!(
        controller
            .web
            .listing
            .as_ref()
            .expect("empty listing")
            .entries
            .is_empty()
    );
    assert_eq!(controller.view.rows.len(), 1);
    assert_eq!(controller.view.rows[0].title, "..");
    assert!(
        controller
            .view
            .status_line
            .contains("No supported audio/video links")
    );
    assert!(controller.view.search_activity.is_none());
    controller.view.search_query = server.url("/missing/").to_string();
    controller.submit_web_url();
    wait_for_web_worker(&mut controller, |_| {});
    assert!(controller.web.listing.is_none());
    assert!(controller.view.rows.is_empty());
    assert!(controller.view.details.is_none());
    assert!(controller.view.status_line.contains("HTTP 404"));
    assert!(controller.view.search_activity.is_none());
    assert_eq!(server.finish(), ["/empty/", "/missing/"]);
}
