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


def main():
	'''Verify notifications, decoded input, and repeated editor/TUI ownership.'''
	master, slave = os.openpty()
	child = None
	transcript = b''
	query_tail = b''
	try:
		resize(slave, 80, 24)
		environment = dict(os.environ, TERM='xterm-256color', YOUTA_INPUT_TEST_SOCKET=sys.argv[2])
		child = subprocess.Popen([
			sys.argv[1], '--exact', 'tui::input_tests::terminal_completion_events_and_editor_handoff', '--nocapture',
		], stdin=slave, stdout=slave, stderr=slave, env=environment, preexec_fn=attach_terminal)
		os.close(slave)
		slave = None

		def wait_for(marker):
			'''Drain bounded terminal output until one distinct fixture marker.'''
			nonlocal transcript, query_tail
			deadline = time.monotonic() + 5
			while marker not in transcript:
				if child.poll() is not None or time.monotonic() >= deadline:
					raise AssertionError(f'Missing {marker!r}: {transcript[-8000:]!r}')
				if not select.select([master], [], [], 0.05)[0]:
					continue
				try:
					chunk = os.read(master, 65536)
					transcript = (transcript + chunk)[-65536:]
					queries = query_tail + chunk
					for _ in range(queries.count(b'\x1b[6n')):
						os.write(master, b'\x1b[1;1R')
					query_tail = queries[-3:]
				except OSError as error:
					if error.errno != errno.EIO:
						raise

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
		assert child.wait(timeout=5) == 0, transcript[-8000:]
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


main()
