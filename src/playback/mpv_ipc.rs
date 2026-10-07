//! The pipe underneath mpv's JSON IPC, on each platform that has one.
//!
//! mpv speaks the same line-delimited JSON protocol everywhere; only the thing
//! the lines travel through changes. On Unix it is a filesystem socket, on
//! Windows a named pipe in the kernel's `\\.\pipe\` namespace. Everything above
//! this module — request framing, event ordering, error mapping, the whole of
//! `mpv.rs` — is written once and shared, so a protocol fix cannot land on one
//! platform and miss the other.
//!
//! One blocking reader hands completed lines to a bounded queue on both
//! platforms. Unsolicited property events wake their consumer without polling
//! mpv, and synchronous command replies retain their bounded timeout. Unix
//! closes the socket and joins its reader on drop; Windows pipe reads end when
//! the owned mpv process exits, as before.
//!
//! The channel is bounded on purpose. A reader that is not being drained blocks
//! on send rather than growing, so a stalled consumer costs a fixed amount of
//! memory instead of an unbounded one.
//!
//! Writes are framed into a single buffer and written once. On a socket this is
//! merely tidy; on a byte-mode pipe it keeps a request from being split across
//! two writes, which is the shape a reader on the far side is least prepared
//! for.

use std::io;
use std::path::Path;
use std::time::Duration;

/// Bounded, independently readable IPC lines with coalescible readiness notices.
struct ReadQueue {
    lines: Option<std::sync::mpsc::Receiver<io::Result<String>>>,
    waker: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    #[cfg_attr(
        windows,
        allow(
            dead_code,
            reason = "Windows pipe reads end when the owned mpv process exits"
        )
    )]
    reader: Option<std::thread::JoinHandle<()>>,
    timeout: Duration,
    closed: bool,
}

impl ReadQueue {
    /// Keeps one blocking reader off the owner so unsolicited events need no request.
    fn start(reader: impl io::Read + Send + 'static, timeout: Duration) -> io::Result<Self> {
        let (sender, lines) = std::sync::mpsc::sync_channel(512);
        let waker = std::sync::Arc::new(std::sync::Mutex::new(None));
        let worker_waker = std::sync::Arc::clone(&waker);
        let reader = std::thread::Builder::new()
            .name("youta-mpv-ipc".to_owned())
            .spawn(move || {
                use std::io::BufRead as _;
                let mut reader = io::BufReader::new(reader);
                loop {
                    let mut line = String::new();
                    let result = reader.read_line(&mut line);
                    let failed = result.is_err();
                    if matches!(result, Ok(0)) {
                        break;
                    }
                    if sender.send(result.map(|_| line)).is_err() {
                        break;
                    }
                    notify(&worker_waker);
                    if failed {
                        break;
                    }
                }
                drop(sender);
                notify(&worker_waker);
            })?;
        Ok(Self {
            lines: Some(lines),
            waker,
            reader: Some(reader),
            timeout,
            closed: false,
        })
    }

    /// Installs readiness after enqueueing and also covers already-buffered lines.
    fn set_waker(&self, waker: Option<std::task::Waker>) {
        let previous = std::mem::replace(
            &mut *self
                .waker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            waker,
        );
        drop(previous);
        notify(&self.waker);
    }

    /// Reads a complete queued line; `Some("")` denotes a disconnected stream.
    fn try_read_line(&mut self) -> io::Result<Option<String>> {
        if self.closed {
            return Ok(Some(String::new()));
        }
        match self.lines.as_ref().expect("attached IPC reader").try_recv() {
            Ok(Ok(line)) => Ok(Some(line)),
            Ok(Err(error)) => {
                self.closed = true;
                Err(error)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.closed = true;
                Ok(Some(String::new()))
            }
        }
    }

    /// Bounds synchronous command acknowledgement without timing out idle readers.
    fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
        if self.closed {
            return Ok(0);
        }
        match self
            .lines
            .as_ref()
            .expect("attached IPC reader")
            .recv_timeout(self.timeout)
        {
            Ok(Ok(next)) => {
                let count = next.len();
                line.push_str(&next);
                Ok(count)
            }
            Ok(Err(error)) => {
                self.closed = true;
                Err(error)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.closed = true;
                Ok(0)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "mpv did not answer its control channel in time",
            )),
        }
    }

    /// Releases backpressure before joining a reader whose transport was shut down.
    #[cfg(unix)]
    fn join(&mut self) {
        drop(self.lines.take());
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Calls user readiness handlers outside the registration lock.
fn notify(waker: &std::sync::Mutex<Option<std::task::Waker>>) {
    let waker = waker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(waker) = waker {
        waker.wake();
    }
}

#[cfg(unix)]
pub(super) use unix_socket::IpcLink;
#[cfg(windows)]
pub(super) use windows_pipe::IpcLink;

/// Opens the control channel mpv published at `endpoint`.
///
/// `timeout` bounds a single read or write once the channel is open; it does
/// not bound the connection attempt, which the caller retries.
///
/// # Errors
///
/// Returns the underlying error when the endpoint is absent, refuses the
/// connection, or cannot be configured.
pub(super) fn connect(endpoint: &Path, timeout: Duration) -> io::Result<IpcLink> {
    #[cfg(unix)]
    {
        unix_socket::connect(endpoint, timeout)
    }
    #[cfg(windows)]
    {
        windows_pipe::connect(endpoint, timeout)
    }
}

/// Reports whether a failed connection attempt means "not yet" rather than "no".
///
/// mpv creates its listening endpoint a moment after the process starts, so the
/// first attempts are expected to fail. Every other failure is real and is
/// reported instead of being retried until the deadline.
#[must_use]
pub(super) fn connection_is_pending(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    }
    #[cfg(windows)]
    {
        /// `ERROR_PIPE_BUSY`: mpv created the pipe, but every instance of it is
        /// already handed out. Retrying is exactly right.
        const ERROR_PIPE_BUSY: i32 = 231;

        error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(ERROR_PIPE_BUSY)
    }
}

#[cfg(unix)]
mod unix_socket {
    use std::io::{self, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    /// One open mpv control channel, carrying whole lines in both directions.
    pub(in crate::playback) struct IpcLink {
        writer: UnixStream,
        lines: super::ReadQueue,
    }

    pub(super) fn connect(endpoint: &Path, timeout: Duration) -> io::Result<IpcLink> {
        let stream = UnixStream::connect(endpoint)?;
        stream.set_write_timeout(Some(timeout))?;
        IpcLink::try_over(stream, timeout)
    }

    impl IpcLink {
        /// Wraps an already connected socket, for tests that supply both ends.
        #[cfg(test)]
        pub(in crate::playback) fn over(stream: UnixStream) -> Self {
            Self::try_over(stream, Duration::from_secs(2)).expect("start mock IPC reader")
        }

        /// Separates blocking reads from commands while preserving socket timeouts.
        fn try_over(stream: UnixStream, timeout: Duration) -> io::Result<Self> {
            stream.set_read_timeout(None)?;
            let lines = super::ReadQueue::start(stream.try_clone()?, timeout)?;
            Ok(Self {
                writer: stream,
                lines,
            })
        }

        /// Registers notifications for complete lines and channel disconnection.
        pub(in crate::playback) fn set_waker(&self, waker: Option<std::task::Waker>) {
            self.lines.set_waker(waker);
        }

        /// Rearms bounded consumers when parsed or unread events still need service.
        pub(in crate::playback) fn wake(&self) {
            super::notify(&self.lines.waker);
        }

        /// Takes one complete line without issuing an IPC command or blocking.
        pub(in crate::playback) fn try_read_line(&mut self) -> io::Result<Option<String>> {
            self.lines.try_read_line()
        }

        /// Writes one request, newline included, as a single write.
        pub(in crate::playback) fn write_line(&mut self, payload: &[u8]) -> io::Result<()> {
            let stream = &mut self.writer;
            stream.write_all(&super::framed(payload))?;
            stream.flush()
        }

        /// Reads the next line, appending it to `line`; zero means closed.
        pub(in crate::playback) fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
            self.lines.read_line(line)
        }
    }

    impl Drop for IpcLink {
        fn drop(&mut self) {
            let _ = self.writer.shutdown(std::net::Shutdown::Both);
            self.lines.join();
        }
    }
}

#[cfg(windows)]
mod windows_pipe {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::Path;
    use std::time::Duration;

    /// One open mpv control channel, carrying whole lines in both directions.
    pub(in crate::playback) struct IpcLink {
        writer: File,
        lines: super::ReadQueue,
    }

    pub(super) fn connect(endpoint: &Path, timeout: Duration) -> io::Result<IpcLink> {
        // A named pipe is opened exactly like a file; the duplicate handle is
        // what lets the reader thread block while this one writes.
        let writer = OpenOptions::new().read(true).write(true).open(endpoint)?;
        let reader = writer.try_clone()?;
        let lines = super::ReadQueue::start(reader, timeout)?;
        Ok(IpcLink { writer, lines })
    }

    impl IpcLink {
        /// Registers notifications for complete lines and channel disconnection.
        pub(in crate::playback) fn set_waker(&self, waker: Option<std::task::Waker>) {
            self.lines.set_waker(waker);
        }

        /// Rearms bounded consumers when parsed or unread events still need service.
        pub(in crate::playback) fn wake(&self) {
            super::notify(&self.lines.waker);
        }

        /// Takes one complete line without issuing an IPC command or blocking.
        pub(in crate::playback) fn try_read_line(&mut self) -> io::Result<Option<String>> {
            self.lines.try_read_line()
        }
        /// Writes one request, newline included, as a single write.
        pub(in crate::playback) fn write_line(&mut self, payload: &[u8]) -> io::Result<()> {
            self.writer.write_all(&super::framed(payload))?;
            self.writer.flush()
        }

        /// Reads the next line, appending it to `line`; zero means closed.
        pub(in crate::playback) fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
            self.lines.read_line(line)
        }
    }
}

/// Appends the protocol's line terminator so a request is one write.
fn framed(payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(payload.len() + 1);
    framed.extend_from_slice(payload);
    framed.push(b'\n');
    framed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_terminated_by_exactly_one_newline() {
        assert_eq!(framed(b"{\"command\":[]}"), b"{\"command\":[]}\n");
        assert_eq!(framed(b""), b"\n");
    }

    #[test]
    fn a_missing_endpoint_is_worth_retrying_and_a_refusal_of_access_is_not() {
        let absent = io::Error::from(io::ErrorKind::NotFound);
        let refused = io::Error::from(io::ErrorKind::PermissionDenied);

        assert!(connection_is_pending(&absent));
        assert!(!connection_is_pending(&refused));
    }

    /// Notifications follow complete lines, never partial JSON fragments.
    #[cfg(unix)]
    #[test]
    fn unsolicited_lines_and_disconnect_wake_without_any_request() {
        use std::io::Write as _;
        use std::sync::{Arc, mpsc};
        use std::task::{Wake, Waker};
        struct Notice(mpsc::Sender<()>);
        impl Wake for Notice {
            fn wake(self: Arc<Self>) {
                let _ = self.0.send(());
            }
        }
        let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut link = IpcLink::over(client);
        let (notice, received) = mpsc::channel();
        link.set_waker(Some(Waker::from(Arc::new(Notice(notice)))));
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(link.try_read_line().unwrap().is_none());
        server.write_all(b"{\"event\":").unwrap();
        assert!(received.recv_timeout(Duration::from_millis(25)).is_err());
        server.write_all(b"\"file-loaded\"}\n").unwrap();
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            link.try_read_line().unwrap().as_deref(),
            Some("{\"event\":\"file-loaded\"}\n")
        );
        drop(server);
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(link.try_read_line().unwrap().as_deref(), Some(""));
    }

    /// Closing a local link releases a reader blocked on an otherwise idle peer.
    #[cfg(unix)]
    #[test]
    fn dropping_a_link_cancels_its_idle_reader() {
        let (client, _server) = std::os::unix::net::UnixStream::pair().unwrap();
        let link = IpcLink::over(client);
        let (done, completion) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            drop(link);
            done.send(()).unwrap();
        });
        completion
            .recv_timeout(Duration::from_secs(2))
            .expect("idle read cancelled");
        thread.join().unwrap();
    }
}
