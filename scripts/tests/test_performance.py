"""Offline contracts for baseline shape, procfs parsing, and PTY cleanup."""

import copy
import importlib.util
import json
import math
from pathlib import Path
import subprocess
import signal
import sys
import tempfile
import time
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location('performance', Path(__file__).parents[1] / 'performance.py')
performance = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(performance)


class PerformanceTests(unittest.TestCase):
	"""Check correctness only: slow but finite measurements remain valid."""

	def report(self):
		"""Return the smallest complete, public-only report fixture."""
		fixture = performance.load_fixture()
		return {
			'schema_version': 1,
			'mode': 'report-only',
			'build': {'expected_profile': 'release', 'expected_features': ['tui'], 'default_features': False,
				'revision': 'a' * 40, 'dirty': False, 'rustc': 'rustc fixture'},
			'fixture': {'definition': fixture, 'sha256': 'b' * 64, 'probe_sources_sha256': 'c' * 64},
			'probes': {
				name: {'scope': scope, 'iterations_per_sample': fixture[iterations],
					'compiled_features': performance.COMPILED_FEATURES, 'debug_assertions': False,
					'description_utf8_bytes': 1000,
					'build_revision': 'a' * 40, 'fixture_definition': fixture,
					'samples_seconds_per_operation': [0.001] * fixture['samples']}
				for name, scope, iterations in [
					('navigation', 'controller-only', 'navigation_iterations'),
					('description_render', 'test-backend-only', 'render_iterations'),
				]
			},
			'process': {'scope': 'empty-state reduced TUI process; child only', 'samples': [
				{'startup_seconds': 0.01, 'idle_wall_seconds': 1.0,
					'idle_cpu_seconds': 0.0, 'idle_cpu_percent_single_core': 0.0,
					'rss_bytes': 123456}
				for _ in range(fixture['process_samples'])]},
		}

	def test_complete_report_accepts_zero_cpu_and_arbitrarily_slow_timings(self):
		report = self.report()
		report['probes']['navigation']['samples_seconds_per_operation'][0] = 1e12
		performance.validate_report(report)

	def test_report_rejects_missing_fields_nonfinite_negative_and_boolean_metrics(self):
		for value in [math.nan, math.inf, -0.1, True, '0.1']:
			with self.subTest(value=value):
				report = self.report()
				report['process']['samples'][0]['startup_seconds'] = value
				with self.assertRaises(ValueError):
					performance.validate_report(report)
		report = self.report()
		del report['process']['samples'][0]['rss_bytes']
		with self.assertRaises(ValueError):
			performance.validate_report(report)

	def test_report_requires_expected_sample_counts_profile_and_fixture_hashes(self):
		for section, key, value in [
			('build', 'expected_features', ['app']), ('build', 'expected_profile', 'dev'),
			('fixture', 'sha256', 'not a digest'), ('process', 'samples', []),
		]:
			report = self.report()
			report[section][key] = value
			with self.assertRaises(ValueError):
				performance.validate_report(report)

	def test_proc_stat_counts_only_child_cpu_and_handles_parentheses_in_name(self):
		fields = ['S'] + ['0'] * 21
		fields[11:15] = ['7', '3', '999', '999']
		fields[21] = '20'
		cpu, rss = performance.parse_proc_stat('42 (fixture ) name) ' + ' '.join(fields), 100, 4096)
		self.assertEqual(cpu, 0.1)
		self.assertEqual(rss, 81920)

	def test_summary_keeps_units_and_reports_all_samples(self):
		self.assertEqual(performance.summarize([3, 1, 2]), {'min': 1, 'median': 2, 'max': 3})

	def test_stale_probe_recipe_or_revision_is_rejected(self):
		for key, value in [('build_revision', 'd' * 40), ('fixture_definition', {})]:
			report = self.report()
			report['probes']['navigation'][key] = value
			with self.assertRaisesRegex(ValueError, 'stale probe'):
				performance.validate_report(report)

	def test_stale_embedded_sources_are_rejected_and_not_published(self):
		with tempfile.TemporaryDirectory() as temporary:
			directory = Path(temporary)
			for name, probe in self.report()['probes'].items():
				probe['source_definitions'] = {'old.rs': 'stale source'}
				(directory / f'{name}.json').write_text(json.dumps(probe))
			with self.assertRaisesRegex(ValueError, 'stale probe sources'):
				performance.load_probes(directory)

	def test_terminal_output_retention_is_bounded_and_split_queries_are_answered(self):
		reader = performance.TerminalReader(42)
		chunks = [b'x' * 65536 + b'\x1b[', b'6nVideo search']
		with mock.patch.object(performance.select, 'select', return_value=([42], [], [])):
			with mock.patch.object(performance.os, 'read', side_effect=chunks):
				with mock.patch.object(performance.os, 'write') as write:
					reader.drain(0)
					reader.drain(0)
		self.assertEqual(len(reader.tail), 65536)
		self.assertTrue(reader.tail.endswith(b'Video search'))
		write.assert_called_once_with(42, b'\x1b[1;1R')

	def test_cleanup_signals_remaining_group_members_after_leader_exit(self):
		child = mock.Mock(pid=1234)
		child.poll.return_value = 0
		with mock.patch.object(performance.os, 'killpg') as kill:
			performance.stop_process(child)
		self.assertEqual(kill.call_args_list, [mock.call(1234, signal.SIGTERM), mock.call(1234, signal.SIGKILL)])
		child.wait.assert_called()

	def test_build_phase_fixes_profile_snapshots_binary_and_uses_fresh_probes(self):
		with tempfile.TemporaryDirectory() as temporary:
			with mock.patch.object(performance.subprocess, 'run') as run:
				with mock.patch.object(performance.shutil, 'copy2') as copy:
					run.return_value.stdout = json.dumps(self.artifact())
					with mock.patch.object(performance, 'metadata', side_effect=[self.artifact()['package_id'], 'host: x86_64-unknown-linux-gnu']):
						binary, probes, build = performance.build_fixtures(Path(temporary))
			self.assertEqual(binary, Path(temporary) / 'youta')
			self.assertEqual(probes, Path(temporary) / 'probes')
			copy.assert_called_once_with(performance.ROOT / 'target/release/youta', binary)
			self.assertEqual(len(run.call_args_list), 2)
			for call in run.call_args_list:
				command = call.args[0]
				self.assertIn('--release', command)
				self.assertIn('--no-default-features', command)
				self.assertEqual(command[command.index('--features') + 1], 'tui')
			self.assertIn('--bin', run.call_args_list[0].args[0])
			self.assertIn('--ignored', run.call_args_list[1].args[0])
			self.assertEqual(run.call_args_list[1].kwargs['env']['YOUTA_PERFORMANCE_PROBE_DIR'], str(probes))
			self.assertEqual(build['compiled_features'], performance.COMPILED_FEATURES)

	def artifact(self):
		"""One native optimized Cargo compiler-artifact record, without building."""
		return {
			'reason': 'compiler-artifact', 'package_id': 'path+file:///fixture#youta@1.0',
			'target': {'name': 'youta', 'kind': ['bin'], 'src_path': str(performance.ROOT / 'src/main.rs')},
			'executable': str(performance.ROOT / 'target/release/youta'),
			'profile': {'opt_level': '3', 'debug_assertions': False, 'test': False},
			'features': performance.COMPILED_FEATURES,
		}

	def test_compiler_artifact_rejects_wrong_package_target_profile_and_features(self):
		for key, value in [
			('package_id', 'other-package'), ('features', ['app']),
			('profile', {'opt_level': '0', 'debug_assertions': False, 'test': False}),
			('profile', {'opt_level': '3', 'debug_assertions': True, 'test': False}),
			('executable', str(performance.ROOT / 'target/aarch64-unknown-linux-gnu/release/youta')),
		]:
			artifact = self.artifact()
			artifact[key] = value
			with self.subTest(key=key, value=value):
				with self.assertRaises(ValueError):
					performance.select_build_artifact(json.dumps(artifact), self.artifact()['package_id'], 'x86_64-unknown-linux-gnu')

	@unittest.skipUnless(sys.platform == 'linux', 'Linux process-group cleanup')
	def test_exited_leader_does_not_leave_a_sigterm_ignoring_grandchild_running(self):
		fixture = copy.deepcopy(performance.load_fixture())
		fixture['readiness_timeout_seconds'] = 2
		with tempfile.TemporaryDirectory() as temporary:
			pid_file = Path(temporary) / 'grandchild.pid'
			grandchild = (
				'import os, signal, time; from pathlib import Path; '
				'signal.signal(signal.SIGTERM, signal.SIG_IGN); '
				'signal.signal(signal.SIGHUP, signal.SIG_IGN); '
				f'Path({str(pid_file)!r}).write_text(str(os.getpid())); time.sleep(60)'
			)
			leader = (
				'import subprocess, sys, time; from pathlib import Path; '
				f'subprocess.Popen([sys.executable, "-c", {grandchild!r}]); '
				f'path = Path({str(pid_file)!r})\n'
				'for _ in range(200):\n'
				'\tif path.exists(): break\n'
				'\ttime.sleep(0.005)\n'
			)
			with self.assertRaisesRegex(RuntimeError, 'readiness'):
				performance.measure_process([sys.executable, '-c', leader], fixture)
			pid = int(pid_file.read_text())
			try:
				deadline = time.monotonic() + 2
				while time.monotonic() < deadline:
					stat = Path(f'/proc/{pid}/stat')
					# An orphan may briefly await the container init's reap, but
					# must not retain a running/sleeping process after cleanup.
					if not stat.exists() or stat.read_text().rsplit(')', 1)[1].split()[0] == 'Z':
						break
					time.sleep(0.01)
				else:
					self.fail('grandchild survived process-group cleanup')
			finally:
				try:
					performance.os.kill(pid, signal.SIGKILL)
				except ProcessLookupError:
					pass

	@unittest.skipUnless(sys.platform == 'linux', 'Linux PTY/procfs runner')
	def test_ready_mock_process_produces_finite_child_samples_and_exits_cleanly(self):
		fixture = copy.deepcopy(performance.load_fixture())
		fixture.update(idle_seconds=0.02, settle_seconds=0.01)
		command = [sys.executable, '-c',
			'import os, tty; tty.setraw(0); os.write(1, b"Video search"); os.read(0, 1)']
		sample = performance.measure_process(command, fixture)
		self.assertTrue(all(performance.finite_metric(sample[key]) for key in performance.PROCESS_METRICS))
		self.assertGreater(sample['rss_bytes'], 0)

	@unittest.skipUnless(sys.platform == 'linux', 'Linux PTY/procfs runner')
	def test_readiness_failure_terminates_and_reaps_child_without_hanging(self):
		fixture = copy.deepcopy(performance.load_fixture())
		fixture['readiness_timeout_seconds'] = 0.1
		children = []
		original_popen = subprocess.Popen

		def spawn(*args, **kwargs):
			child = original_popen(*args, **kwargs)
			children.append(child)
			return child

		with mock.patch.object(performance.subprocess, 'Popen', side_effect=spawn):
			with self.assertRaisesRegex(RuntimeError, 'readiness'):
				performance.measure_process([sys.executable, '-c', 'import time; time.sleep(60)'], fixture)
		self.assertEqual(len(children), 1)
		self.assertIsNotNone(children[0].returncode)


if __name__ == '__main__':
	unittest.main()
