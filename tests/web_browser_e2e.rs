//! Opt-in integration check against Python's actual directory-listing server.
//!
//! Run with `cargo test --locked --features web-browser --test web_browser_e2e -- --ignored`.
//! Requires `python3` on PATH, but no external network, yt-dlp, mpv, or real
//! audio decoding. Small opaque fixture files exercise discovery by filename.

#![cfg(feature = "web-browser")]

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use url::Url;
use youta::web_browser::{WebBrowserClient, WebEntryKind};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_STARTUP_OUTPUT_BYTES: u64 = 4 * 1024;

/// Owns the exact Python child and reaps it even when a test assertion panics.
struct PythonHttpServer {
    child: Child,
    startup_reader: Option<JoinHandle<()>>,
}

impl PythonHttpServer {
    /// Starts Python on an OS-assigned loopback port and bounds its announcement.
    fn start(directory: &Path) -> (Self, Url) {
        let child = Command::new("python3")
            .args([
                "-u",
                "-m",
                "http.server",
                "0",
                "--bind",
                "127.0.0.1",
                "--directory",
            ])
            .arg(directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("this ignored integration test requires python3 on PATH");
        let mut server = Self {
            child,
            startup_reader: None,
        };
        let stdout = server.child.stdout.take().expect("piped Python stdout");
        let (ready, started) = mpsc::channel();
        server.startup_reader = Some(thread::spawn(move || {
            let reader = BufReader::new(stdout).take(MAX_STARTUP_OUTPUT_BYTES);
            for line in reader.lines() {
                let Ok(line) = line else {
                    break;
                };
                // Python prints: Serving HTTP on 127.0.0.1 port N (...).
                if let Some(port) = line
                    .strip_prefix("Serving HTTP on 127.0.0.1 port ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|value| value.parse::<u16>().ok())
                    .filter(|port| *port != 0)
                {
                    let _ = ready.send(Some(port));
                    return;
                }
            }
            let _ = ready.send(None);
        }));
        let port = started
            .recv_timeout(STARTUP_TIMEOUT)
            .expect("python3 -m http.server did not announce its port within five seconds; check its stderr")
            .expect("Python exited or returned an unsupported startup announcement; check its stderr");
        let url = Url::parse(&format!("http://127.0.0.1:{port}/"))
            .expect("announced loopback HTTP address");
        (server, url)
    }
}

impl Drop for PythonHttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.startup_reader.take() {
            let _ = reader.join();
        }
    }
}

/// Browses Python's real HTML output, enters a folder, and returns to its parent.
#[test]
#[ignore = "requires python3; run cargo test --features web-browser --test web_browser_e2e -- --ignored"]
fn python_http_directory_browsing_preserves_encoded_media_and_parent_navigation() {
    let directory = tempfile::tempdir().expect("temporary HTTP document root");
    fs::write(
        directory.path().join("01 chapter & intro.mp3"),
        b"audio fixture",
    )
    .expect("audio filename fixture");
    fs::write(
        directory.path().join("02 clip # demo.MP4"),
        b"video fixture",
    )
    .expect("video filename fixture");
    fs::write(directory.path().join("notes.txt"), b"not playable").expect("unrelated text fixture");
    let nested = directory.path().join("nested audio");
    fs::create_dir(&nested).expect("nested directory");
    fs::write(nested.join("03 nested.opus"), b"nested audio fixture")
        .expect("nested audio filename fixture");

    let (_server, base) = PythonHttpServer::start(directory.path());
    let client = WebBrowserClient::default();
    let listing = client.list(&base).expect("list Python HTTP root");
    assert_eq!(listing.url, base);
    assert!(listing.parent.is_none());
    assert!(!listing.truncated);
    assert_eq!(listing.entries.len(), 3);
    assert_eq!(listing.entries[0].name, "nested audio");
    assert_eq!(listing.entries[0].kind, WebEntryKind::Directory);
    assert_eq!(listing.entries[0].url.path(), "/nested%20audio/");
    assert_eq!(listing.entries[1].name, "01 chapter & intro.mp3");
    assert_eq!(listing.entries[1].kind, WebEntryKind::Audio);
    assert_eq!(
        listing.entries[1].url,
        base.join("01%20chapter%20%26%20intro.mp3").unwrap()
    );
    assert_eq!(listing.entries[2].name, "02 clip # demo.MP4");
    assert_eq!(listing.entries[2].kind, WebEntryKind::Video);
    assert_eq!(
        listing.entries[2].url,
        base.join("02%20clip%20%23%20demo.MP4").unwrap()
    );
    assert!(listing.entries[2].url.fragment().is_none());
    assert!(
        listing
            .entries
            .iter()
            .all(|entry| entry.name != "notes.txt")
    );

    let child = client
        .list(&listing.entries[0].url)
        .expect("enter Python subdirectory");
    assert_eq!(child.entries.len(), 1);
    assert_eq!(child.entries[0].name, "03 nested.opus");
    assert_eq!(child.entries[0].kind, WebEntryKind::Audio);
    assert_eq!(
        child.entries[0].url.path(),
        "/nested%20audio/03%20nested.opus"
    );
    let parent = child.parent.expect("subdirectory has a parent");
    assert_eq!(parent, base);
    let restored = client
        .list(&parent)
        .expect("return to Python parent directory");
    assert_eq!(restored.entries, listing.entries);
}
