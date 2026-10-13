'''Drive an isolated Rust input test; no user configuration or network access.'''

import errno
import fcntl
import os
import select
import signal
import socket
import struct
import subprocess
import sys
import termios
import time


def attach_terminal():
	'''Give the test child a controlling terminal and its own process group.'''
	os.setsid()
	fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def resize(fd, columns, rows):
	'''Let the kernel deliver SIGWINCH to the terminal's foreground process.'''
	fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))


class TerminalOutput:
	'''Retain bounded PTY output and answer the child's cursor-position queries.'''

	def __init__(self, master, child):
		'''Keep one transcript across distinct event and editor handoff markers.'''
		self.master = master
		self.child = child
		self.transcript = b''
		self.query_tail = b''

	def wait_for(self, marker):
		'''Drain ready output before interpreting process exit as a missing marker.'''
		deadline = time.monotonic() + 5
		while marker not in self.transcript:
			remaining = max(0, deadline - time.monotonic())
			if select.select([self.master], [], [], min(0.05, remaining))[0]:
				try:
					chunk = os.read(self.master, 65536)
					if not chunk:
						self.missing(marker, 'PTY closed (EOF)')
				except OSError as error:
					if error.errno != errno.EIO:
						raise
					self.missing(marker, 'PTY closed (EIO)')
				self.transcript = (self.transcript + chunk)[-65536:]
				queries = self.query_tail + chunk
				for _ in range(queries.count(b'\x1b[6n')):
					os.write(self.master, b'\x1b[1;1R')
				self.query_tail = queries[-3:]
				if marker in self.transcript:
					return
			elif self.child.poll() is not None:
				# The child may have flushed and exited after the first readiness check.
				# Once exit is confirmed, a final nonblocking check sees all its output.
				if select.select([self.master], [], [], 0)[0]:
					continue
				self.missing(marker, 'child exited')
			if time.monotonic() >= deadline:
				self.missing(marker, 'timed out')

	def missing(self, marker, reason):
		'''Distinguish watchdog, terminal closure, and process-exit failures.'''
		status = self.child.poll()
		raise AssertionError(
			f'Missing {marker!r} ({reason}; child status={status!r}): {self.transcript[-8000:]!r}'
		)


def main():
	'''Verify notifications, decoded input, and repeated editor/TUI ownership.'''
	master, slave = os.openpty()
	child = None
	try:
		resize(slave, 80, 24)
		environment = dict(os.environ, TERM='xterm-256color', YOUTA_INPUT_TEST_SOCKET=sys.argv[2])
		child = subprocess.Popen([
			sys.argv[1], '--exact', 'tui::input_tests::terminal_completion_events_and_editor_handoff', '--nocapture',
		], stdin=slave, stdout=slave, stderr=slave, env=environment, preexec_fn=attach_terminal)
		os.close(slave)
		slave = None

		output = TerminalOutput(master, child)
		wait_for = output.wait_for

		wait_for(b'WORKER_WAIT_READY')
		with socket.socket(socket.AF_UNIX) as worker:
			worker.connect(sys.argv[2])
			worker.sendall(b'w')
		wait_for(b'INPUT_READY')
		os.write(master, b'\x1b[A\x1b[<0;5;6M')
		wait_for(b'RESIZE_READY')
		resize(master, 100, 30)
		for cycle in range(2):
			wait_for(f'EDITOR_READY_{cycle}'.encode())
			os.write(master, f'editor-owns-keys-{cycle}\n'.encode())
			wait_for(f'EDITOR_RETURNED_{cycle}'.encode())
			os.write(master, b'q')
		wait_for(b'INPUT_TEST_DONE')
		assert child.wait(timeout=5) == 0, output.transcript[-8000:]
	finally:
		if child is not None:
			try:
				os.killpg(child.pid, signal.SIGKILL)
			except ProcessLookupError:
				pass
			child.wait(timeout=5)
		os.close(master)
		if slave is not None:
			os.close(slave)


if __name__ == '__main__':
	main()
