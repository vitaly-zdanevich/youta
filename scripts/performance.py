#!/usr/bin/env python3
"""Report offline Linux baselines; build/probe commands are documented in README.

The initial, unmeasured phase builds a fixed reduced release profile and runs
the fixture probes into a fresh directory. Measurement then uses a snapshot of
that executable directly, in a fresh config/working directory and a minimal
environment. PTY output is drained continuously and retained only as a bounded
readiness buffer, never written into the public report. Deadlines protect the
runner from hangs; they are not performance regression thresholds.
"""

import argparse
import errno
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import select
import signal
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parents[1]
FIXTURE_PATH = ROOT / 'tests/fixtures/performance.json'
PROBE_SOURCES = [
	'src/app/tests/performance.rs', 'src/tui/performance.rs',
	'src/test_support/performance.rs', 'scripts/performance.py',
]
COMPILED_FEATURES = ['cli', 'controller', 'subscriptions', 'tui']
PROCESS_METRICS = [
	'startup_seconds', 'idle_wall_seconds', 'idle_cpu_seconds',
	'idle_cpu_percent_single_core', 'rss_bytes',
]


def load_fixture():
	"""Read the single versioned recipe shared with the Rust fixture probes."""
	return json.loads(FIXTURE_PATH.read_text(encoding='utf-8'))


def parse_proc_stat(text, ticks_per_second, page_bytes):
	"""Read this process's user+system CPU and resident pages, excluding children."""
	fields = text.rsplit(')', 1)[1].split()
	return (int(fields[11]) + int(fields[12])) / ticks_per_second, int(fields[21]) * page_bytes


def process_usage(pid):
	"""Sample the actual TUI PID, not Cargo, a shell, or cumulative child CPU."""
	return parse_proc_stat(
		Path(f'/proc/{pid}/stat').read_text(encoding='utf-8'),
		os.sysconf('SC_CLK_TCK'), os.sysconf('SC_PAGE_SIZE'),
	)


def attach_terminal():
	"""Make the PTY slave the controlling terminal of this single-threaded child."""
	os.setsid()
	fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class TerminalReader:
	"""Bound retained output while servicing Crossterm cursor-position queries."""

	def __init__(self, descriptor):
		self.descriptor = descriptor
		self.tail = b''
		self.query_tail = b''

	def drain(self, timeout):
		"""Read at most one bounded chunk; the caller owns the overall deadline."""
		if not select.select([self.descriptor], [], [], max(0, timeout))[0]:
			return
		try:
			chunk = os.read(self.descriptor, 65536)
		except OSError as error:
			if error.errno == errno.EIO:
				return
			raise
		self.tail = (self.tail + chunk)[-65536:]
		queries = self.query_tail + chunk
		for _ in range(queries.count(b'\x1b[6n')):
			os.write(self.descriptor, b'\x1b[1;1R')
		self.query_tail = queries[-3:]


def signal_group(pid, requested_signal):
	"""Signal the owned process group even when its leader has already exited."""
	try:
		os.killpg(pid, requested_signal)
	except ProcessLookupError:
		pass


def stop_process(child):
	"""Terminate the isolated group on every failure and always reap its leader."""
	signal_group(child.pid, signal.SIGTERM)
	try:
		child.wait(timeout=2)
	except subprocess.TimeoutExpired:
		pass
	# A group member may ignore SIGTERM or outlive the leader. Always finish
	# group cleanup, including after a clean exit of the direct child.
	signal_group(child.pid, signal.SIGKILL)
	child.wait(timeout=2)


def drain_until(reader, child, deadline):
	"""Drain idle output without allowing a full PTY buffer to stall the child."""
	while time.perf_counter() < deadline:
		if child.poll() is not None:
			raise RuntimeError('TUI exited before the idle observation completed')
		reader.drain(min(0.05, deadline - time.perf_counter()))


def measure_process(command, fixture):
	"""Measure one fresh empty-state process after the recognizable first frame.

	Only the child receives TERM/locale and an empty helper PATH. No user config,
	credentials, current directory, terminal transcript, or environment is saved.
	The required reduced `tui` build contains no source/playback network adapters.
	"""
	with tempfile.TemporaryDirectory(prefix='youta-performance-') as temporary:
		working = Path(temporary)
		helpers = working / 'empty-helpers'
		helpers.mkdir()
		master, slave = os.openpty()
		child = None
		try:
			fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack(
				'HHHH', fixture['terminal_rows'], fixture['terminal_columns'], 0, 0))
			started = time.perf_counter()
			child = subprocess.Popen(
				[*command, '--config-dir', str(working / 'config')],
				stdin=slave, stdout=slave, stderr=slave, cwd=working,
				env={'TERM': 'xterm-256color', 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8', 'PATH': str(helpers)},
				preexec_fn=attach_terminal,
			)
			os.close(slave)
			slave = None
			reader = TerminalReader(master)
			deadline = started + fixture['readiness_timeout_seconds']
			while b'Video search' not in reader.tail:
				if child.poll() is not None or time.perf_counter() >= deadline:
					raise RuntimeError('TUI readiness marker was not reached before exit/deadline')
				reader.drain(min(0.05, deadline - time.perf_counter()))
			startup = time.perf_counter() - started
			drain_until(reader, child, time.perf_counter() + fixture['settle_seconds'])
			cpu_before, _ = process_usage(child.pid)
			idle_started = time.perf_counter()
			drain_until(reader, child, idle_started + fixture['idle_seconds'])
			cpu_after, rss = process_usage(child.pid)
			idle_wall = time.perf_counter() - idle_started
			cpu = cpu_after - cpu_before
			os.write(master, b'q')
			exit_deadline = time.perf_counter() + 5
			while child.poll() is None and time.perf_counter() < exit_deadline:
				reader.drain(0.05)
			if child.poll() != 0:
				raise RuntimeError('TUI did not exit cleanly after the observation')
			return {
				'startup_seconds': startup, 'idle_wall_seconds': idle_wall,
				'idle_cpu_seconds': cpu, 'idle_cpu_percent_single_core': cpu / idle_wall * 100,
				'rss_bytes': rss,
			}
		finally:
			try:
				if child is not None:
					stop_process(child)
			finally:
				os.close(master)
				if slave is not None:
					os.close(slave)


def require(condition, message):
	"""Raise a schema error, never a speed-regression failure."""
	if not condition:
		raise ValueError(message)


def finite_metric(value):
	"""Zero is meaningful at procfs tick resolution; booleans are not metrics."""
	return type(value) in (int, float) and math.isfinite(value) and value >= 0


def validate_report(report):
	"""Require complete, comparable, finite samples without timing thresholds."""
	try:
		require(report['schema_version'] == 1 and report['mode'] == 'report-only', 'report schema/mode')
		build = report['build']
		require(build['expected_profile'] == 'release' and build['expected_features'] == ['tui']
			and build['default_features'] is False, 'reduced release build required')
		require(re.fullmatch(r'[0-9a-f]{40,64}', build['revision']) is not None, 'revision')
		require(type(build['dirty']) is bool and bool(build['rustc']), 'build metadata')
		fixture = report['fixture']
		require(fixture['definition'] == load_fixture(), 'fixture definition mismatch')
		for key in ['sha256', 'probe_sources_sha256']:
			require(re.fullmatch(r'[0-9a-f]{64}', fixture[key]) is not None, key)
		definition = fixture['definition']
		for name, scope, iterations in [
			('navigation', 'controller-only', 'navigation_iterations'),
			('description_render', 'test-backend-only', 'render_iterations'),
		]:
			probe = report['probes'][name]
			require(probe['build_revision'] == build['revision'], 'stale probe revision')
			require(probe['fixture_definition'] == definition, 'stale probe fixture')
			require(probe['scope'] == scope, 'probe scope')
			require(probe['compiled_features'] == COMPILED_FEATURES
				and probe['debug_assertions'] is False, 'probe build mismatch')
			require(probe['iterations_per_sample'] == definition[iterations], 'probe iteration count')
			samples = probe['samples_seconds_per_operation']
			require(len(samples) == definition['samples'] and all(map(finite_metric, samples)), 'probe samples')
			require(type(probe['description_utf8_bytes']) is int and probe['description_utf8_bytes'] > 0, 'description size')
		samples = report['process']['samples']
		require(report['process']['scope'] == 'empty-state reduced TUI process; child only', 'process scope')
		require(len(samples) == definition['process_samples'], 'process sample count')
		for sample in samples:
			require(all(finite_metric(sample[key]) for key in PROCESS_METRICS), 'process metrics')
	except (KeyError, TypeError, AttributeError) as error:
		raise ValueError('incomplete or malformed performance report') from error


def summarize(samples):
	"""Retain min/median/max alongside the raw samples for future comparisons."""
	return {'min': min(samples), 'median': statistics.median(samples), 'max': max(samples)}


def load_probes(directory):
	"""Reject measurements compiled from stale probe sources before publication."""
	expected = {name: (ROOT / name).read_text(encoding='utf-8') for name in PROBE_SOURCES if name.endswith('.rs')}
	probes = {}
	for name in ['navigation', 'description_render']:
		probe = json.loads((directory / f'{name}.json').read_text(encoding='utf-8'))
		require(probe.pop('source_definitions', None) == expected, 'stale probe sources; rebuild and rerun fixtures')
		probes[name] = probe
	return probes


def file_digest(path):
	"""Hash a binary without loading the executable into runner memory at once."""
	with path.open('rb') as stream:
		return hashlib.file_digest(stream, 'sha256').hexdigest()


def metadata(command):
	"""Collect an allowlisted public tool/revision value, never a full environment."""
	return subprocess.check_output(command, cwd=ROOT, text=True, timeout=10).strip()


def markdown(report):
	"""Render a compact, explicitly scoped summary without machine-specific paths."""
	lines = [
		'# Offline performance baseline', '',
		'Reporting only; no timing regression thresholds. Build time is excluded.', '',
		f"Revision: `{report['build']['revision']}` (dirty: {report['build']['dirty']}).",
		'Expected process profile: `release --no-default-features --features tui`; not the full default application.',
		'Process features/profile are bound by this runner’s fixed unmeasured build and executable snapshot; probe features are checked.',
		f"Toolchain: `{report['build']['rustc'].splitlines()[0]}`.", '',
		'| Measurement | Unit | Minimum | Median | Maximum |',
		'| --- | --- | ---: | ---: | ---: |',
	]
	for name, probe in report['probes'].items():
		values = summarize(probe['samples_seconds_per_operation'])
		lines.append(f"| {name} ({probe['scope']}) | seconds/operation | "
			+ ' | '.join(f'{value:.9g}' for value in values.values()) + ' |')
	for key in PROCESS_METRICS:
		values = summarize([sample[key] for sample in report['process']['samples']])
		unit = 'bytes' if key == 'rss_bytes' else ('% of one CPU' if 'percent' in key else 'seconds')
		lines.append(f'| Process {key} | {unit} | '
			+ ' | '.join(f'{value:.9g}' for value in values.values()) + ' |')
	lines.extend([
		'', f"Fixture recipe SHA-256: `{report['fixture']['sha256']}`.",
		f"Probe source SHA-256: `{report['fixture']['probe_sources_sha256']}`.", '',
		'Navigation includes selection dispatch and preliminary details, not rendering or network requests.',
		'Rendering uses Ratatui TestBackend with a fixed large list and wrapped description, not terminal I/O.',
		f"Process startup ends at the first recognizable Video search frame in a drained {report['fixture']['definition']['terminal_columns']}×{report['fixture']['definition']['terminal_rows']} PTY.",
		'Idle CPU/RSS sample only that child after readiness and settling; RSS is the end sample, not peak RSS.',
		'Empty-state startup is not populated-library startup. No playback, images, GUI, or network adapters are enabled.',
		'CPU is quantized to procfs ticks; zero means below that observation resolution. Shared-runner noise remains.',
		'Raw samples, fixture sizes, binary hash, and public host/toolchain metadata are in report.json.', '',
	])
	return '\n'.join(lines)


def build_fixtures(directory):
	"""Build the fixed profile before timing, snapshot it, and generate fresh probes.

	No arbitrary executable or old probe input is accepted. The snapshot prevents
	other Cargo feature builds from replacing the executable while it is measured.
	Build dependencies may be fetched here; the measured phase is fully offline.
	"""
	arguments = ['--locked', '--release', '--no-default-features', '--features', 'tui',
		'--target-dir', str(ROOT / 'target')]
	package = metadata(['cargo', 'pkgid', '--locked', '--package', 'youta'])
	host = next(line.removeprefix('host: ') for line in metadata(['rustc', '-vV']).splitlines() if line.startswith('host: '))
	completed = subprocess.run(['cargo', 'build', *arguments, '--bin', 'youta',
		'--message-format=json-render-diagnostics'], cwd=ROOT, check=True, timeout=21600,
		stdout=subprocess.PIPE, text=True)
	executable, build = select_build_artifact(completed.stdout, package, host)
	binary = directory / 'youta'
	shutil.copy2(executable, binary)
	probes = directory / 'probes'
	environment = dict(os.environ, YOUTA_PERFORMANCE_PROBE_DIR=str(probes))
	subprocess.run(['cargo', 'test', *arguments, '--lib', 'performance_fixture_probe',
		'--', '--ignored', '--test-threads=1'], cwd=ROOT, env=environment, check=True, timeout=21600)
	return binary, probes, build


def select_build_artifact(output, package, host):
	"""Bind measurements to Cargo's exact native binary, not a guessed stale path."""
	records = [json.loads(line) for line in output.splitlines() if line.strip()]
	artifacts = [record for record in records if record.get('reason') == 'compiler-artifact'
		and record.get('package_id') == package and record.get('target', {}).get('name') == 'youta'
		and record['target'].get('kind') == ['bin']]
	require(len(artifacts) == 1, 'expected exactly one youta package binary artifact')
	artifact = artifacts[0]
	require(Path(artifact['target']['src_path']).resolve() == ROOT / 'src/main.rs', 'wrong binary source')
	profile = artifact['profile']
	require(profile['opt_level'] == '3' and profile['debug_assertions'] is False
		and profile['test'] is False, 'wrong executable optimization profile')
	require(sorted(artifact['features']) == COMPILED_FEATURES, 'wrong executable features')
	executable = Path(artifact['executable']).resolve()
	native_paths = [ROOT / 'target/release/youta', ROOT / 'target' / host / 'release/youta']
	require(executable in native_paths, 'expected a native executable; remove cross-target Cargo configuration')
	return executable, {'compiled_features': sorted(artifact['features']),
		'opt_level': profile['opt_level'], 'debug_assertions': profile['debug_assertions'], 'target': host}


def collect_report(binary, probe_directory, executable_build):
	"""Validate fresh probe identities before measuring the just-built process."""
	fixture = load_fixture()
	probes = load_probes(probe_directory)
	report = {
		'schema_version': 1, 'mode': 'report-only',
		'build': {'expected_profile': 'release', 'expected_features': ['tui'], 'default_features': False,
			'process_build_verification': 'Cargo compiler-artifact and executable snapshot',
			'process_build': executable_build,
			'revision': metadata(['git', 'rev-parse', 'HEAD']),
			'dirty': bool(metadata(['git', 'status', '--porcelain', '--untracked-files=normal'])),
			'rustc': metadata(['rustc', '-vV']), 'binary_sha256': file_digest(binary)},
		'host': {'system': platform.system(), 'architecture': platform.machine(),
			'kernel': platform.release(), 'python': platform.python_version(), 'logical_cpus': os.cpu_count(),
			'procfs_ticks_per_second': os.sysconf('SC_CLK_TCK')},
		'fixture': {'definition': fixture, 'sha256': file_digest(FIXTURE_PATH),
			'probe_sources_sha256': hashlib.sha256(b''.join(
				name.encode() + b'\0' + (ROOT / name).read_bytes() + b'\0' for name in PROBE_SOURCES)).hexdigest()},
		'probes': probes,
		'process': {'scope': 'empty-state reduced TUI process; child only',
			'samples': [dict.fromkeys(PROCESS_METRICS, 0) for _ in range(fixture['process_samples'])]},
	}
	# Reject stale or wrong-feature probes before starting any process. Only
	# actual observations replace these shape-validation placeholders below.
	validate_report(report)
	report['process']['samples'] = [measure_process([str(binary)], fixture)
		for _ in range(fixture['process_samples'])]
	validate_report(report)
	return report


def main():
	"""Build outside measurement, then publish only complete validated reports."""
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument('--output-dir', type=Path, required=True)
	arguments = parser.parse_args()
	if sys.platform != 'linux':
		parser.error('the process baseline requires Linux PTY and procfs')
	with tempfile.TemporaryDirectory(prefix='youta-performance-build-') as temporary:
		binary, probes, build = build_fixtures(Path(temporary))
		report = collect_report(binary, probes, build)
	arguments.output_dir.mkdir(parents=True, exist_ok=True)
	(arguments.output_dir / 'report.json').write_text(json.dumps(report, indent='\t', allow_nan=False) + '\n', encoding='utf-8')
	(arguments.output_dir / 'report.md').write_text(markdown(report), encoding='utf-8')
	print('Validated report.json and report.md (report-only; no timing thresholds).')


if __name__ == '__main__':
	main()
