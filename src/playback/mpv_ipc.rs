//! The pipe underneath mpv's JSON IPC, on each platform that has one.
//!
//! mpv speaks the same line-delimited JSON protocol everywhere; only the thing
//! the lines travel through changes. On Unix it is a filesystem socket, on
//! Windows a named pipe in the kernel's `\\.\pipe\` namespace. Everything above
//! this module — request framing, event ordering, error mapping, the whole of
//! `mpv.rs` — is written once and shared, so a protocol fix cannot land on one
//! platform and miss the other.
//!
//! # Why the two halves are not symmetric
//!
//! A Unix socket answers `set_read_timeout`, so the two-second guard against an
//! mpv that stops replying is one syscall. A Windows named pipe opened as a
//! file has no such control: the documented ways to bound a blocking read are
//! overlapped I/O and `PeekNamedPipe`, both of which mean raw Win32 calls, and
//! this crate forbids `unsafe` outright. So the Windows half buys the same
//! guarantee with a thread: one reader owns the pipe, hands finished lines to a
//! bounded channel, and the caller's timeout becomes `recv_timeout`. The thread
//! ends when the pipe closes, which is what mpv exiting does, and `shutdown`
//! kills mpv, so no reader outlives the backend that started it.
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
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    /// One open mpv control channel, carrying whole lines in both directions.
    pub(in crate::playback) struct IpcLink {
        reader: BufReader<UnixStream>,
    }

    pub(super) fn connect(endpoint: &Path, timeout: Duration) -> io::Result<IpcLink> {
        let stream = UnixStream::connect(endpoint)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(IpcLink::over(stream))
    }

    impl IpcLink {
        /// Wraps an already connected socket, for tests that supply both ends.
        pub(in crate::playback) fn over(stream: UnixStream) -> Self {
            Self {
                reader: BufReader::new(stream),
            }
        }

        /// Writes one request, newline included, as a single write.
        pub(in crate::playback) fn write_line(&mut self, payload: &[u8]) -> io::Result<()> {
            let stream = self.reader.get_mut();
            stream.write_all(&super::framed(payload))?;
            stream.flush()
        }

        /// Reads the next line, appending it to `line`; zero means closed.
        pub(in crate::playback) fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
            self.reader.read_line(line)
        }
    }
}

#[cfg(windows)]
mod windows_pipe {
    use std::fs::{File, OpenOptions};
    use std::io::{self, BufRead, BufReader, Write};
    use std::path::Path;
    use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
    use std::thread;
    use std::time::Duration;

    /// Lines the reader may hold before it blocks instead of allocating.
    ///
    /// mpv emits one line per reply and per event; a consumer that is keeping
    /// up never approaches this, and one that is not stops the reader rather
    /// than growing the queue without limit.
    const PENDING_LINES: usize = 512;

    /// One open mpv control channel, carrying whole lines in both directions.
    pub(in crate::playback) struct IpcLink {
        writer: File,
        lines: Receiver<io::Result<String>>,
        timeout: Duration,
        closed: bool,
    }

    pub(super) fn connect(endpoint: &Path, timeout: Duration) -> io::Result<IpcLink> {
        // A named pipe is opened exactly like a file; the duplicate handle is
        // what lets the reader thread block while this one writes.
        let writer = OpenOptions::new().read(true).write(true).open(endpoint)?;
        let reader = writer.try_clone()?;
        let (sender, lines) = sync_channel(PENDING_LINES);
        thread::Builder::new()
            .name("youta-mpv-ipc".to_owned())
            .spawn(move || pump(reader, &sender))?;
        Ok(IpcLink {
            writer,
            lines,
            timeout,
            closed: false,
        })
    }

    /// Turns blocking reads into lines on a channel until the pipe closes.
    fn pump(reader: File, sender: &SyncSender<io::Result<String>>) {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                // mpv closed the pipe. Dropping the sender is the message.
                Ok(0) => return,
                Ok(_) => {
                    if sender.send(Ok(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
    }

    impl IpcLink {
        /// Writes one request, newline included, as a single write.
        pub(in crate::playback) fn write_line(&mut self, payload: &[u8]) -> io::Result<()> {
            self.writer.write_all(&super::framed(payload))?;
            self.writer.flush()
        }

        /// Reads the next line, appending it to `line`; zero means closed.
        pub(in crate::playback) fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
            if self.closed {
                return Ok(0);
            }
            match self.lines.recv_timeout(self.timeout) {
                Ok(Ok(next)) => {
                    let read = next.len();
                    line.push_str(&next);
                    Ok(read)
                }
                Ok(Err(error)) => {
                    self.closed = true;
                    Err(error)
                }
                Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "mpv did not answer its control pipe in time",
                )),
                // The reader stopped, which only happens once the pipe is gone.
                Err(RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    Ok(0)
                }
            }
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
}
