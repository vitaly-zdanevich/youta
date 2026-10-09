#!/usr/bin/env python3
'''Install the local offline badge hook and its pinned development parsers.'''

import os
from pathlib import Path
import subprocess
import sys
import venv


MARKER = '# Youta badge hook; managed by scripts/install_git_hooks.py'
LAUNCHER = f'''#!/bin/sh
{MARKER}
root=$(git rev-parse --show-toplevel) || exit 1
environment=$(git rev-parse --git-path production-loc-venv) || exit 1
python="$environment/bin/python"
if [ ! -x "$python" ]; then
	python="$environment/Scripts/python.exe"
fi
if [ ! -x "$python" ]; then
	printf '%s\\n' 'Youta badge hook: run python3 scripts/install_git_hooks.py' >&2
	exit 1
fi
exec "$python" "$root/scripts/pre_commit_badges.py"
'''


def repository_path(root, name):
	'''Locate the effective hooks directory or a persistent private environment.'''
	output = subprocess.check_output(['git', 'rev-parse', '--path-format=absolute', '--git-path', name], cwd=root)
	return Path(os.fsdecode(output).strip())


def prepare_environment(environment, requirements):
	'''Install pinned parsers only during explicit setup, never during a commit.'''
	venv.EnvBuilder(with_pip=True).create(environment)
	python = environment / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')
	subprocess.run([str(python), '-m', 'pip', 'install', '-r', str(requirements)], check=True)


def install(root):
	'''Install idempotently without overwriting another tool's hook or Git config.'''
	root = Path(root).resolve()
	hook = repository_path(root, 'hooks/pre-commit')
	common_output = subprocess.check_output(['git', 'rev-parse', '--path-format=absolute', '--git-common-dir'], cwd=root)
	common = Path(os.fsdecode(common_output).strip()).resolve()
	if root not in hook.resolve().parents and hook.resolve() != common / 'hooks/pre-commit':
		raise RuntimeError(f'hooks directory is outside this repository: {hook.parent}; integrate the badge command into the shared hook explicitly')
	if hook.is_symlink() or (hook.exists() and (not hook.is_file() or MARKER not in hook.read_text(encoding='utf-8'))):
		raise RuntimeError(f'refusing to replace existing hook {hook}; add a call to scripts/pre_commit_badges.py using the parser environment to your existing hook instead')
	prepare_environment(repository_path(root, 'production-loc-venv'), root / 'scripts/production-loc-requirements.txt')
	hook.parent.mkdir(parents=True, exist_ok=True)
	hook.write_text(LAUNCHER, encoding='utf-8', newline='\n')
	hook.chmod(hook.stat().st_mode | 0o111)
	print(f'Installed automatic staged-code badges: {hook}')


def main():
	'''Configure this checkout while leaving pre-existing hook ownership intact.'''
	try:
		root = Path(os.fsdecode(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'])).strip())
		install(root)
	except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
		print(f'Youta hook setup: {error}', file=sys.stderr)
		return 1
	return 0


if __name__ == '__main__':
	sys.exit(main())
