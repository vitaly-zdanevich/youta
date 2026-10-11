#!/usr/bin/env python3
'''Probe public radio services independently; never hide exhausted failures.

This POSIX diagnostic compiles the existing Rust integration target once, checks
its test inventory, then retries only the failing probe. It intentionally lives
outside the deterministic CI/release gates: a stream outage remains a failed
health check, not a deterministic playback regression.
'''

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
PROBE_TIMEOUT = 90
RETRY_DELAYS = (2, 5)
CHECKS = (
	'radio_somafm_playlist_audio_and_icy_metadata_are_usable',
	'radio_palitra_stream_declares_mp3',
	'radio_kalx_berkeley_flac_audio_is_usable',
	'radio_calico_flac_audio_is_usable',
	'radio_intense_flac_audio_is_usable',
	'radio_openbroadcast_flac_audio_is_usable',
	'radio_bergeijk_flac_audio_is_usable',
	'radio_pure_classix_flac_audio_is_usable',
	'radio_campus_grenoble_flac_audio_is_usable',
	'radio_sector_progressive_flac_audio_is_usable',
	'radio_rlocale_flac_audio_is_usable',
	'radio_1zwolle_flac_audio_is_usable',
	'radio_sega_opus_audio_is_usable',
	'radio_four_duk_now_playing_is_usable',
	'radio_sector_now_playing_is_usable',
	'generated_npr_station_stream_and_program_are_usable',
	'bbc_sounds_resolution_and_audio_are_usable',
)


def run_process(command, *, timeout, env=None, stdout=None):
	'''Bound a trusted helper and reap its whole process group, including mpv.'''
	process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=stdout, start_new_session=True)
	try:
		return process.wait(timeout=timeout)
	except subprocess.TimeoutExpired:
		print(f'Process exceeded {timeout}s: {command[0]}', file=sys.stderr, flush=True)
		return 124
	finally:
		# A failed Rust assertion can leave an mpv descendant even after its
		# parent exits. Only terminate the private group created above.
		try:
			os.killpg(process.pid, signal.SIGKILL)
		except ProcessLookupError:
			pass
		process.wait()


def build_executable():
	'''Compile once and select the integration executable from Cargo JSON.'''
	command = [
		'cargo', 'test', '--locked', '--no-default-features',
		'--features', 'radio,bbc-radio,backend-mpv', '--test', 'live_services',
		'--no-run', '--message-format=json',
	]
	with tempfile.TemporaryFile(mode='w+', encoding='utf-8') as output:
		if run_process(command, timeout=900, stdout=output) != 0:
			output.seek(0)
			for line in output:
				print(line, end='', file=sys.stderr)
			raise RuntimeError('building radio health probes failed')
		output.seek(0)
		artifacts = []
		for line in output:
			message = json.loads(line)
			if (message.get('reason') == 'compiler-artifact'
				and message.get('target', {}).get('name') == 'live_services'
				and 'test' in message.get('target', {}).get('kind', [])
				and message.get('executable')):
				artifacts.append(Path(message['executable']))
	if len(artifacts) != 1:
		raise RuntimeError('Cargo did not report exactly one live-services executable')
	return artifacts[0]


def validate_checks(executable, checks):
	'''Reject renamed or missing tests: libtest otherwise passes zero matches.'''
	listing = subprocess.run(
		[str(executable), '--ignored', '--list', '--format', 'terse'],
		cwd=ROOT, check=True, capture_output=True, text=True, timeout=15,
	).stdout
	available = {line.removesuffix(': test') for line in listing.splitlines() if line.endswith(': test')}
	missing = set(checks) - available
	if missing:
		raise RuntimeError(f'missing radio health tests: {", ".join(sorted(missing))}')


def run_probe(executable, name):
	'''Enable public-network access only for one explicitly requested test.'''
	environment = {
		**os.environ,
		'YOUTA_RUN_LIVE_RADIO_TEST': '1',
		'YOUTA_RUN_LIVE_BBC_RADIO_TEST': '1',
	}
	return run_process(
		[str(executable), '--ignored', '--exact', name, '--nocapture'],
		timeout=PROBE_TIMEOUT, env=environment,
	) == 0


def check_all(checks, attempt, wait):
	'''Retry each failed check twice, then continue regardless of its outcome.'''
	results = []
	for name in checks:
		for index in range(len(RETRY_DELAYS) + 1):
			if index:
				wait(RETRY_DELAYS[index - 1])
			print(f'Radio health: {name}, attempt {index + 1}/3', flush=True)
			passed = attempt(name)
			if passed:
				break
		results.append((name, passed, index + 1))
	return results


def exit_status(results):
	'''An empty or partly failed run cannot be presented as a healthy catalogue.'''
	return 0 if results and all(passed for _, passed, _ in results) else 1


def write_summary(results, path=None):
	'''Report every outcome to stdout and optionally GitHub's job summary.'''
	lines = [
		'## Radio station health', '',
		'A public-service failure is not a deterministic playback regression.', '',
		'| Check | Result | Attempts |', '| --- | --- | --- |',
	]
	for name, passed, attempts in results:
		status = 'PASS' if passed else 'FAIL'
		lines.append(f'| {name} | {status} | {attempts} |')
	text = '\n'.join(lines) + '\n'
	print(text, flush=True)
	if path is not None:
		with Path(path).open('a', encoding='utf-8') as output:
			output.write(text)


def main(argv=None):
	'''Require opt-in, compile once, validate the inventory and run all checks.'''
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument('--allow-network', action='store_true', help='allow public HTTP(S) radio probes')
	parser.add_argument('--check', action='append', choices=CHECKS, help='probe only this check (repeatable)')
	arguments = parser.parse_args(argv)
	if not arguments.allow_network:
		parser.error('--allow-network is required for public radio probes')
	if os.name != 'posix':
		parser.error('this diagnostic requires POSIX process-group cleanup')
	try:
		checks = tuple(dict.fromkeys(arguments.check or CHECKS))
		executable = build_executable()
		validate_checks(executable, checks)
		results = check_all(checks, lambda name: run_probe(executable, name), time.sleep)
		write_summary(results, os.environ.get('GITHUB_STEP_SUMMARY'))
		return exit_status(results)
	except (OSError, RuntimeError, subprocess.SubprocessError) as error:
		print(f'Radio health infrastructure error: {error}', file=sys.stderr)
		return 1


if __name__ == '__main__':
	sys.exit(main())
