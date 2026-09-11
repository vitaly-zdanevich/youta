//! Bounded HTTP socket polls without ambiguous blocking timeout progress.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use super::IO_POLL;

/// Lets a blocked socket become ready without spinning or delaying successful I/O.
const SOCKET_RETRY_BACKOFF: Duration = Duration::from_millis(5);

/// Owns a nonblocking socket while exposing bounded polls to HTTP workers.
///
/// A blocking Winsock send timeout leaves the connection in an indeterminate
/// state: bytes may already have been accepted without a returned byte count.
/// Retrying that buffer can duplicate audio. Nonblocking I/O reports accepted
/// prefixes immediately; only `WouldBlock` is retried, with a 5-ms backoff that
/// avoids busy-spinning. Each poll returns control within [`IO_POLL`] so the
/// caller can check cancellation and its response or media-idle deadline.
///
/// See <https://learn.microsoft.com/en-us/windows/win32/winsock/sol-socket-socket-options>.
pub(super) struct HttpStream {
    socket: TcpStream,
}

impl HttpStream {
    /// Sets a known socket mode instead of depending on platform inheritance.
    pub(super) fn new(socket: TcpStream) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        socket.set_read_timeout(None)?;
        socket.set_write_timeout(None)?;
        Ok(Self { socket })
    }

    /// Clones the already-configured socket without changing its shared options.
    pub(super) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            socket: self.socket.try_clone()?,
        })
    }
}

impl Read for HttpStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        poll_nonblocking(|| self.socket.read(bytes))
    }
}

impl Write for HttpStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        poll_nonblocking(|| self.socket.write(bytes))
    }

    fn flush(&mut self) -> io::Result<()> {
        poll_nonblocking(|| self.socket.flush())
    }
}

/// Uses short sleeps only while a nonblocking socket cannot make progress.
fn poll_nonblocking<T>(operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    poll_nonblocking_with_policy(operation, Instant::now, thread::sleep)
}

/// Injects time and waiting so backpressure policy is testable without real sleeps.
///
/// Successful partial operations and all errors other than `WouldBlock` return
/// unchanged. The outer HTTP policy owns cancellation, interruption retries,
/// and whole-operation deadlines; this helper bounds only one socket poll.
fn poll_nonblocking_with_policy<T>(
    mut operation: impl FnMut() -> io::Result<T>,
    mut now: impl FnMut() -> Instant,
    mut wait: impl FnMut(Duration),
) -> io::Result<T> {
    let deadline = now() + IO_POLL;
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(now());
                if remaining.is_zero() {
                    return Err(error);
                }
                wait(SOCKET_RETRY_BACKOFF.min(remaining));
                if now() >= deadline {
                    return Err(error);
                }
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::net::{Ipv4Addr, TcpListener};

    #[test]
    fn socket_poll_returns_exact_partial_progress_without_waiting() {
        for accepted in [0, 3, 8] {
            let started = Instant::now();
            let clock = Cell::new(started);
            let mut calls = 0;
            let mut waits = Vec::new();
            let mut output = Vec::new();
            let bytes = b"abcdefgh";
            let written = poll_nonblocking_with_policy(
                || {
                    calls += 1;
                    output.extend_from_slice(&bytes[..accepted]);
                    Ok(accepted)
                },
                || clock.get(),
                |duration| {
                    waits.push(duration);
                    clock.set(clock.get() + duration);
                },
            )
            .expect("partial success and EOF must return immediately");
            assert_eq!(written, accepted);
            assert_eq!(output, &bytes[..accepted]);
            assert_eq!(calls, 1);
            assert!(waits.is_empty());
            assert_eq!(clock.get(), started);
        }
    }

    #[test]
    fn socket_poll_waits_only_until_readiness_then_returns_partial_progress() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let mut waits = Vec::new();
        let mut steps = VecDeque::from([
            Err(io::ErrorKind::WouldBlock.into()),
            Err(io::ErrorKind::WouldBlock.into()),
            Ok(3),
        ]);
        let result = poll_nonblocking_with_policy(
            || steps.pop_front().expect("bounded operation script"),
            || clock.get(),
            |duration| {
                waits.push(duration);
                clock.set(clock.get() + duration);
            },
        )
        .expect("readiness after backpressure must succeed");
        assert_eq!(result, 3);
        assert!(steps.is_empty());
        assert_eq!(waits, [SOCKET_RETRY_BACKOFF; 2]);
        assert_eq!(clock.get() - started, SOCKET_RETRY_BACKOFF * 2);
    }

    #[test]
    fn socket_poll_bounds_stalls_to_one_cancellation_interval() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let mut calls = 0;
        let mut waits = Vec::new();
        let error = poll_nonblocking_with_policy(
            || {
                calls += 1;
                Err::<usize, _>(io::ErrorKind::WouldBlock.into())
            },
            || clock.get(),
            |duration| {
                waits.push(duration);
                clock.set(clock.get() + duration);
            },
        )
        .expect_err("a stalled socket must return control to its caller");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(clock.get() - started, IO_POLL);
        assert_eq!(calls, 20);
        assert_eq!(waits, [SOCKET_RETRY_BACKOFF; 20]);
    }

    #[test]
    fn socket_poll_clips_its_last_wait_to_the_remaining_budget() {
        let started = Instant::now();
        let clock = Cell::new(started);
        let mut calls = 0;
        let mut waits = Vec::new();
        let error = poll_nonblocking_with_policy(
            || {
                calls += 1;
                // Account for time spent in the socket call, not just backoff.
                clock.set(clock.get() + Duration::from_millis(3));
                Err::<usize, _>(io::ErrorKind::WouldBlock.into())
            },
            || clock.get(),
            |duration| {
                waits.push(duration);
                clock.set(clock.get() + duration);
            },
        )
        .expect_err("polling must not sleep beyond its remaining budget");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(calls, 13);
        assert_eq!(clock.get() - started, IO_POLL);
        assert_eq!(&waits[..12], &[SOCKET_RETRY_BACKOFF; 12]);
        assert_eq!(waits.last(), Some(&Duration::from_millis(1)));
    }

    #[test]
    fn socket_poll_does_not_retry_or_wait_after_terminal_errors() {
        for kind in [
            io::ErrorKind::Interrupted,
            io::ErrorKind::TimedOut,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
        ] {
            let mut calls = 0;
            let error = poll_nonblocking_with_policy(
                || {
                    calls += 1;
                    Err::<usize, _>(io::Error::new(kind, "terminal socket failure"))
                },
                Instant::now,
                |_| panic!("terminal errors must not schedule another socket attempt"),
            )
            .expect_err("terminal socket errors must remain terminal");
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), "terminal socket failure");
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn http_stream_clears_socket_timeouts_and_preserves_configuration_on_clone() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("test listener");
        let _client = TcpStream::connect(listener.local_addr().expect("test address"))
            .expect("connect test client");
        let (socket, _) = listener.accept().expect("accept test client");
        socket
            .set_read_timeout(Some(IO_POLL))
            .expect("initial read timeout");
        socket
            .set_write_timeout(Some(IO_POLL))
            .expect("initial write timeout");
        let stream = HttpStream::new(socket).expect("configure HTTP socket");
        let clone = stream.try_clone().expect("clone configured socket");
        for configured in [&stream, &clone] {
            assert_eq!(configured.socket.read_timeout().unwrap(), None);
            assert_eq!(configured.socket.write_timeout().unwrap(), None);
        }
    }
}
