'''Verify foreground Local commands in an isolated PTY without user configuration.'''

import errno
import fcntl
import os
import select
import signal
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path


def attach_terminal():
	'''Give the Rust child a controlling terminal and its own process group.'''
	os.setsid()
	fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def resize(fd, columns, rows):
	'''Generate a real resize event that must not dismiss command output.'''
	fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))


def stop_descendants(parent_pid):
	'''Stop only this fixture's descendants, including separate shell job groups.'''
	parents = {}
	for entry in Path('/proc').iterdir():
		if not entry.name.isdecimal():
			continue
		try:
			for line in (entry / 'status').read_text().splitlines():
				if line.startswith('PPid:'):
					parents[int(entry.name)] = int(line.split()[1])
					break
		except (OSError, ValueError):
			continue
	owned = [parent_pid]
	for parent in owned:
		owned.extend(pid for pid, ppid in parents.items() if ppid == parent and pid not in owned)
	for pid in reversed(owned):
		try:
			os.kill(pid, signal.SIGKILL)
		except ProcessLookupError:
			pass


def main():
	'''Check shell IO, key dismissal, Ctrl-C, and repeated exclusive TTY ownership.'''
	master, slave = os.openpty()
	child = None
	transcript = b''
	query_tail = b''
	cursor = 0
	try:
		resize(slave, 80, 24)
		environment = dict(os.environ, TERM='xterm-256color', YOUTA_LOCAL_COMMAND_TEST_DIRECTORY=sys.argv[2])
		# The runner must not source user shell configuration or save commands in it.
		environment.pop('BASH_ENV', None)
		environment.pop('ENV', None)
		environment['HISTFILE'] = os.devnull
		child = subprocess.Popen([
			sys.argv[1], '--exact', 'tui::local_command_tests::local_commands_preserve_output_and_restore_terminal_input', '--nocapture',
		], stdin=slave, stdout=slave, stderr=slave, env=environment, preexec_fn=attach_terminal)
		os.close(slave)
		slave = None

		def drain(timeout):
			'''Read bounded terminal output and answer terminal-position queries.'''
			nonlocal transcript, query_tail, cursor
			if not select.select([master], [], [], timeout)[0]:
				return
			try:
				chunk = os.read(master, 65536)
			except OSError as error:
				if error.errno == errno.EIO:
					return
				raise
			transcript += chunk
			if len(transcript) > 131072:
				raise AssertionError('Local command fixture exceeded its output limit')
			queries = query_tail + chunk
			for _ in range(queries.count(b'\x1b[6n')):
				os.write(master, b'\x1b[1;1R')
			query_tail = queries[-3:]

		def wait_for(marker):
			'''Wait for the next marker, never reusing a prior command's prompt.'''
			nonlocal cursor
			deadline = time.monotonic() + 8
			while marker not in transcript[cursor:]:
				if child.poll() is not None or time.monotonic() >= deadline:
					raise AssertionError(f'Missing {marker!r}, status={child.poll()}: {transcript[-12000:]!r}')
				drain(0.05)
			start = transcript.index(marker, cursor)
			captured = transcript[cursor:start]
			cursor = start + len(marker)
			return captured

		def wait_for_foreground_sleep():
			'''Confirm exec replaced the inner shell before sending terminal Ctrl-C.'''
			deadline = time.monotonic() + 5
			while time.monotonic() < deadline:
				try:
					foreground = os.tcgetpgrp(master)
					if Path(f'/proc/{foreground}/comm').read_text().strip() == 'sleep':
						return
				except OSError:
					pass
				drain(0.02)
			raise AssertionError(f'Foreground exec sleep did not start: {transcript[-12000:]!r}')

		for index, name in enumerate(('normal', 'exec', 'nonzero', 'validation', 'missing_directory', 'interrupt', 'read')):
			wait_for(f'COMMAND_BEGIN_{name}'.encode())
			if name == 'interrupt':
				wait_for(b'COMMAND_INTERRUPT_READY')
				wait_for_foreground_sleep()
				os.write(master, b'\x03')
			elif name == 'read':
				wait_for(b'COMMAND_INPUT_READY')
				os.write(master, b'command owns input\n')
			output = wait_for(b'Press any key to return to Youta')
			if name == 'normal':
				for marker in (b'PATH_ONE_ARG_OK', b'COMMAND_STDOUT', b'COMMAND_STDERR'):
					assert marker in output, (marker, output)
			elif name == 'exec':
				assert b'COMMAND_EXEC_OK' in output, output
			elif name == 'read':
				assert b'COMMAND_INPUT_OK' in output, output
			elif name == 'validation':
				assert b'supports simple unquoted arguments only' in output, output
			elif name == 'missing_directory':
				assert b'cannot run Bash' in output, output
				assert b'UNEXPECTED_SPAWN' not in output, output
			assert os.tcgetpgrp(master) == child.pid, 'Bash did not restore the original foreground group'
			returned = f'COMMAND_RETURNED_{name}'.encode()
			resize(master, 100 + index, 30 + index)
			deadline = time.monotonic() + 0.2
			while time.monotonic() < deadline:
				drain(0.02)
				assert returned not in transcript[cursor:], 'command returned without a key press'
			os.write(master, b'x')
			wait_for(returned)
			os.write(master, b'q')
			wait_for(f'COMMAND_DONE_{name}'.encode())
		wait_for(b'LOCAL_COMMAND_TEST_DONE')
		assert child.wait(timeout=5) == 0, transcript[-12000:]
	finally:
		if child is not None:
			if child.poll() is None:
				stop_descendants(child.pid)
			child.wait(timeout=5)
		os.close(master)
		if slave is not None:
			os.close(slave)


main()
