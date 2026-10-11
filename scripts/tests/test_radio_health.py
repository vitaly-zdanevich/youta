'''Offline checks for bounded, independent public radio probes.'''

import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location('radio_health', Path(__file__).parents[1] / 'radio_health.py')
health = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(health)


class RadioHealthTests(unittest.TestCase):
	'''A broken station must neither skip later checks nor be reported as healthy.'''

	def test_retries_only_failed_check_and_continues_after_exhaustion(self):
		calls = []
		waits = []
		outcomes = iter([False, False, False, True, False, True])

		def attempt(name):
			calls.append(name)
			return next(outcomes)

		results = health.check_all(['broken', 'healthy', 'transient'], attempt, waits.append)
		self.assertEqual(calls, ['broken', 'broken', 'broken', 'healthy', 'transient', 'transient'])
		self.assertEqual(waits, [2, 5, 2])
		self.assertEqual(results, [('broken', False, 3), ('healthy', True, 1), ('transient', True, 2)])
		self.assertEqual(health.exit_status(results), 1)

	def test_all_successes_need_no_wait_and_exit_successfully(self):
		with mock.patch.object(health.time, 'sleep', side_effect=AssertionError('no retry')):
			results = health.check_all(['one', 'two'], lambda _: True, health.time.sleep)
		self.assertEqual(results, [('one', True, 1), ('two', True, 1)])
		self.assertEqual(health.exit_status(results), 0)
		self.assertEqual(health.exit_status([]), 1)

	def test_network_opt_in_is_required_before_building(self):
		with mock.patch.object(health, 'build_executable') as build:
			with mock.patch.object(health.sys, 'stderr', io.StringIO()):
				with self.assertRaises(SystemExit) as failure:
					health.main([])
		self.assertEqual(failure.exception.code, 2)
		build.assert_not_called()

	def test_build_failure_cannot_start_network_checks(self):
		with mock.patch.object(health, 'build_executable', side_effect=RuntimeError('fixture build error')):
			with mock.patch.object(health, 'run_probe') as probe:
				with mock.patch.object(health.sys, 'stderr', io.StringIO()):
					self.assertEqual(health.main(['--allow-network']), 1)
		probe.assert_not_called()

	def test_probe_command_runs_exactly_one_test_with_opt_ins(self):
		with mock.patch.object(health, 'run_process', return_value=0) as run:
			self.assertTrue(health.run_probe(Path('/tmp/live-tests'), 'radio_fixture'))
		command = run.call_args.args[0]
		self.assertEqual(command, ['/tmp/live-tests', '--ignored', '--exact', 'radio_fixture', '--nocapture'])
		self.assertEqual(run.call_args.kwargs['timeout'], health.PROBE_TIMEOUT)
		self.assertEqual(run.call_args.kwargs['env']['YOUTA_RUN_LIVE_RADIO_TEST'], '1')
		self.assertEqual(run.call_args.kwargs['env']['YOUTA_RUN_LIVE_BBC_RADIO_TEST'], '1')

	def test_timeout_is_failure_and_process_group_is_reaped(self):
		process = mock.Mock(pid=123)
		process.wait.side_effect = [subprocess.TimeoutExpired(['fixture'], 90), -9]
		with mock.patch.object(health.subprocess, 'Popen', return_value=process) as popen:
			with mock.patch.object(health.os, 'killpg') as kill:
				self.assertNotEqual(health.run_process(['fixture'], timeout=90), 0)
		self.assertTrue(popen.call_args.kwargs['start_new_session'])
		kill.assert_called_once_with(123, health.signal.SIGKILL)
		self.assertEqual(process.wait.call_count, 2)

	def test_cleanup_includes_descendants_after_successful_leader_exit(self):
		process = mock.Mock(pid=124)
		process.wait.return_value = 0
		with mock.patch.object(health.subprocess, 'Popen', return_value=process):
			with mock.patch.object(health.os, 'killpg') as kill:
				self.assertEqual(health.run_process(['fixture'], timeout=90), 0)
		kill.assert_called_once_with(124, health.signal.SIGKILL)

	def test_missing_test_name_is_rejected_before_network_probes(self):
		with mock.patch.object(health.subprocess, 'run') as run:
			run.return_value.stdout = 'different_test: test\n'
			with self.assertRaisesRegex(RuntimeError, 'missing.*radio_fixture'):
				health.validate_checks(Path('/tmp/live-tests'), ['radio_fixture'])
		self.assertIn('--list', run.call_args.args[0])

	def test_report_records_recovered_and_failed_checks(self):
		with tempfile.TemporaryDirectory() as temporary:
			path = Path(temporary) / 'summary.md'
			health.write_summary([('outage', False, 3), ('recovered', True, 2)], path)
			text = path.read_text()
		self.assertIn('outage | FAIL | 3', text)
		self.assertIn('recovered | PASS | 2', text)
		self.assertIn('not a deterministic playback regression', text)

	def test_every_radio_check_is_unique_and_expected(self):
		self.assertEqual(len(health.CHECKS), 17)
		self.assertEqual(len(set(health.CHECKS)), len(health.CHECKS))
		self.assertIn('radio_sector_progressive_flac_audio_is_usable', health.CHECKS)
		self.assertIn('generated_npr_station_stream_and_program_are_usable', health.CHECKS)
		self.assertIn('bbc_sounds_resolution_and_audio_are_usable', health.CHECKS)


if __name__ == '__main__':
	unittest.main()
