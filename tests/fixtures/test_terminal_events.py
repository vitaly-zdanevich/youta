'''Deterministic coverage for the real-PTY test driver's output handling.'''

import errno
import sys
import types
import unittest
from unittest.mock import Mock, patch


if len(sys.argv) > 1 and sys.argv[1] == '--fixture-source':
	# Cargo embeds both scripts so the checks do not depend on the current folder.
	fixture = types.ModuleType('terminal_events')
	exec(compile(sys.argv[2], 'terminal_events.py', 'exec'), fixture.__dict__)
	del sys.argv[1:3]
else:
	import terminal_events as fixture


class TerminalOutputTests(unittest.TestCase):
	'''Mock process exit and PTY readiness instead of racing their schedulers.'''

	def test_exited_child_buffered_completion_is_drained(self):
		'''Process exit must not hide its flushed but still unread final marker.'''
		child = Mock()
		child.poll.return_value = 0
		output = fixture.TerminalOutput(7, child)
		with patch.object(fixture.select, 'select', return_value=([7], [], [])), \
			patch.object(fixture.os, 'read', return_value=b'INPUT_TEST_DONE\r\n') as read:
			output.wait_for(b'INPUT_TEST_DONE')
		read.assert_called_once_with(7, 65536)
		self.assertIn(b'INPUT_TEST_DONE', output.transcript)

	def test_exited_child_completion_split_across_chunks_is_drained(self):
		'''Keep draining an exited child's output until the marker is complete.'''
		child = Mock()
		child.poll.return_value = 0
		output = fixture.TerminalOutput(7, child)
		with patch.object(fixture.select, 'select', return_value=([7], [], [])), \
			patch.object(fixture.os, 'read', side_effect=[b'INPUT_', b'TEST_DONE\r\n']) as read:
			output.wait_for(b'INPUT_TEST_DONE')
		self.assertEqual(read.call_count, 2)
		child.poll.assert_not_called()

	def test_exit_without_completion_reports_status_and_transcript(self):
		'''An exited child without queued output still fails with useful evidence.'''
		child = Mock()
		child.poll.return_value = 7
		output = fixture.TerminalOutput(7, child)
		output.transcript = b'last event'
		with patch.object(fixture.select, 'select', return_value=([], [], [])), \
			self.assertRaisesRegex(AssertionError, 'child exited; child status=7.*last event'):
			output.wait_for(b'INPUT_TEST_DONE')

	def test_completion_flushed_between_readiness_check_and_exit_is_drained(self):
		'''Recheck output after exit closes the readiness/status ordering window.'''
		child = Mock()
		child.poll.return_value = 0
		output = fixture.TerminalOutput(7, child)
		with patch.object(fixture.select, 'select', side_effect=[
			([], [], []), ([7], [], []), ([7], [], []), ([7], [], []),
		]) as select, \
			patch.object(fixture.os, 'read', side_effect=[b'INPUT_', b'TEST_DONE\r\n']) as read:
			output.wait_for(b'INPUT_TEST_DONE')
		self.assertEqual(read.call_count, 2)
		self.assertEqual(select.call_args_list[1].args, ([7], [], [], 0))

	def test_pty_eof_reports_failure_without_busy_loop(self):
		'''A readable terminal returning no bytes is closed, not another wait.'''
		child = Mock()
		child.poll.return_value = None
		output = fixture.TerminalOutput(7, child)
		with patch.object(fixture.select, 'select', return_value=([7], [], [])), \
			patch.object(fixture.os, 'read', return_value=b'') as read, \
			self.assertRaisesRegex(AssertionError, r'PTY closed \(EOF\); child status=None'):
			output.wait_for(b'INPUT_TEST_DONE')
		read.assert_called_once()

	def test_pty_eio_reports_failure_without_busy_loop(self):
		'''Linux PTY EOF can appear as EIO and must not spin until the watchdog.'''
		child = Mock()
		child.poll.return_value = 3
		output = fixture.TerminalOutput(7, child)
		with patch.object(fixture.select, 'select', return_value=([7], [], [])), \
			patch.object(fixture.os, 'read', side_effect=OSError(errno.EIO, 'closed')) as read, \
			self.assertRaisesRegex(AssertionError, r'PTY closed \(EIO\); child status=3'):
			output.wait_for(b'INPUT_TEST_DONE')
		read.assert_called_once()

	def test_other_read_errors_are_not_hidden(self):
		'''Only the documented PTY-close errno is converted to fixture diagnostics.'''
		output = fixture.TerminalOutput(7, Mock())
		with patch.object(fixture.select, 'select', return_value=([7], [], [])), \
			patch.object(fixture.os, 'read', side_effect=OSError(errno.EBADF, 'bad fd')), \
			self.assertRaises(OSError) as raised:
			output.wait_for(b'INPUT_TEST_DONE')
		self.assertEqual(raised.exception.errno, errno.EBADF)

	def test_watchdog_reports_timeout_for_a_running_child(self):
		'''Advance the mock clock rather than sleeping to verify bounded waits.'''
		child = Mock()
		child.poll.return_value = None
		output = fixture.TerminalOutput(7, child)
		with patch.object(fixture.time, 'monotonic', side_effect=[10, 15, 15]), \
			patch.object(fixture.select, 'select', return_value=([], [], [])) as select, \
			self.assertRaisesRegex(AssertionError, 'timed out; child status=None'):
			output.wait_for(b'INPUT_TEST_DONE')
		select.assert_called_once_with([7], [], [], 0)

	def test_unrelated_ready_output_does_not_extend_the_watchdog(self):
		'''A stream of unrelated terminal bytes cannot postpone the fixed deadline.'''
		child = Mock()
		child.poll.return_value = None
		output = fixture.TerminalOutput(7, child)
		with patch.object(fixture.time, 'monotonic', side_effect=[10, 10, 15]), \
			patch.object(fixture.select, 'select', return_value=([7], [], [])), \
			patch.object(fixture.os, 'read', return_value=b'unrelated output') as read, \
			self.assertRaisesRegex(AssertionError, 'timed out; child status=None.*unrelated output'):
			output.wait_for(b'INPUT_TEST_DONE')
		read.assert_called_once()

	def test_split_cursor_queries_are_answered_once_each(self):
		'''Retain the cursor-query tail across reads without duplicating replies.'''
		output = fixture.TerminalOutput(7, Mock())
		with patch.object(fixture.select, 'select', return_value=([7], [], [])), \
			patch.object(fixture.os, 'read', side_effect=[b'\x1b[6', b'n\x1b[6nDONE']), \
			patch.object(fixture.os, 'write') as write:
			output.wait_for(b'DONE')
		self.assertEqual(write.call_count, 2)
		for call in write.call_args_list:
			self.assertEqual(call.args, (7, b'\x1b[1;1R'))

	def test_an_already_seen_marker_needs_no_additional_read(self):
		'''Markers retained from a preceding read are accepted without touching the PTY.'''
		output = fixture.TerminalOutput(7, Mock())
		output.transcript = b'INPUT_TEST_DONE'
		with patch.object(fixture.select, 'select') as select:
			output.wait_for(b'INPUT_TEST_DONE')
		select.assert_not_called()


if __name__ == '__main__':
	unittest.main()
