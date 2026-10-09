#!/usr/bin/env python3
'''Regenerate the three code badges from the exact index Git will commit.

The counter and source are exported together so partial staging stays intact.
Only generated badge entries are added to the active index; no source is staged.
Dependencies are installed explicitly by scripts/install_git_hooks.py.
'''

from importlib import metadata
import os
from pathlib import Path
import subprocess
import sys
import tempfile


BADGES = (
	Path('docs/badges/production-code.svg'),
	Path('docs/badges/test-code.svg'),
	Path('docs/badges/total-code.svg'),
)
SETUP = 'python3 scripts/install_git_hooks.py'


def git(root, *arguments, input=None):
	'''Run Git while preserving its active index and repository environment.'''
	environment = {**os.environ, 'GIT_NO_LAZY_FETCH': '1'}
	return subprocess.check_output(['git', *arguments], cwd=root, input=input, env=environment, stderr=subprocess.PIPE)


def git_path(root, name):
	'''Resolve repository-local paths, including an inherited temporary index.'''
	return Path(os.fsdecode(git(root, 'rev-parse', '--path-format=absolute', '--git-path', name)).strip())


def index_entries(root):
	'''Read stage-zero blob identities and reject unresolved merge entries.'''
	entries = {}
	for record in git(root, 'ls-files', '--stage', '-z').split(b'\0'):
		if not record:
			continue
		description, name = record.split(b'\t', 1)
		mode, object_id, stage = description.decode('ascii').split()
		if stage != '0':
			raise RuntimeError('resolve staged merge conflicts before regenerating badges')
		entries[Path(os.fsdecode(name))] = (mode, object_id)
	return entries


def check_badge_worktree(root, entries):
	'''Reject unstaged generated-file edits before changing the index or files.'''
	for name in BADGES:
		path = root / name
		if any((root / part).is_symlink() for part in (name, *name.parents)):
			raise RuntimeError(f'badge path must not contain a symbolic link: {name}')
		entry = entries.get(name)
		if entry is not None and entry[0] not in {'100644', '100755'}:
			raise RuntimeError(f'badge must be a regular staged file: {name}')
		staged = git(root, 'cat-file', 'blob', entry[1]) if entry is not None else None
		working = path.read_bytes() if path.exists() else None
		if working != staged:
			raise RuntimeError(f'unstaged badge changes in {name}; stage or save those edits before committing')


def check_dependencies(requirements):
	'''Require the staged, pinned parser versions without fetching packages.'''
	for line in requirements.read_text(encoding='utf-8').splitlines():
		line = line.partition('#')[0].strip()
		if not line:
			continue
		name, separator, expected = line.partition('==')
		if not separator or not name or not expected:
			raise RuntimeError(f'unsupported parser requirement {line!r}; run {SETUP}')
		try:
			installed = metadata.version(name)
		except metadata.PackageNotFoundError:
			installed = None
		if installed != expected:
			raise RuntimeError(f'badge parser requires {line}; run {SETUP}')


def export_index(root, entries, snapshot):
	'''Materialize raw staged blobs without checkout filters or network access.'''
	blobs = []
	for name, (mode, object_id) in entries.items():
		if name.is_absolute() or '..' in name.parts:
			raise RuntimeError(f'unsafe staged path: {name}')
		if mode == '160000':
			(snapshot / name).mkdir(parents=True, exist_ok=True)
		elif mode in {'100644', '100755', '120000'}:
			blobs.append((name, mode, object_id))
		else:
			raise RuntimeError(f'unsupported staged file mode {mode}: {name}')
	contents = git(root, 'cat-file', '--batch', input=''.join(f'{object_id}\n' for _, _, object_id in blobs).encode('ascii'))
	offset = 0
	symlinks = []
	for name, mode, object_id in blobs:
		header_end = contents.index(b'\n', offset)
		returned_id, kind, size = contents[offset:header_end].decode('ascii').split()
		if returned_id != object_id or kind != 'blob':
			raise RuntimeError(f'expected a staged blob for {name}')
		start = header_end + 1
		end = start + int(size)
		if contents[end:end + 1] != b'\n':
			raise RuntimeError(f'incomplete staged blob for {name}')
		content = contents[start:end]
		offset = end + 1
		path = snapshot / name
		path.parent.mkdir(parents=True, exist_ok=True)
		if mode == '120000':
			symlinks.append((path, content))
		else:
			path.write_bytes(content)
	# Symlinks are created last so exporting cannot follow one outside the snapshot.
	for path, content in symlinks:
		path.symlink_to(os.fsdecode(content))


def refresh(root):
	'''Count staged sources and add only the generated SVGs to this commit.'''
	root = Path(root).resolve()
	entries = index_entries(root)
	check_badge_worktree(root, entries)
	with tempfile.TemporaryDirectory(prefix='youta-staged-badges-') as temporary:
		snapshot = Path(temporary)
		export_index(root, entries, snapshot)
		requirements = snapshot / 'scripts/production-loc-requirements.txt'
		counter = snapshot / 'scripts/production_loc.py'
		for name in (*BADGES, Path('scripts/production-loc-requirements.txt'), Path('scripts/production_loc.py')):
			if any((snapshot / part).is_symlink() for part in (name, *name.parents)):
				raise RuntimeError(f'counter and badge paths must not contain symbolic links: {name}')
		if not requirements.is_file() or not counter.is_file():
			raise RuntimeError('stage the source counter and its requirements before committing')
		check_dependencies(requirements)
		environment = os.environ.copy()
		environment.update({
			'GIT_DIR': os.fsdecode(git(root, 'rev-parse', '--absolute-git-dir')).strip(),
			'GIT_WORK_TREE': str(snapshot),
			'GIT_INDEX_FILE': str(git_path(root, 'index')),
		})
		environment.pop('GIT_PREFIX', None)
		subprocess.run([sys.executable, str(counter), '--write'], cwd=snapshot, env=environment, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
		generated = {name: (snapshot / name).read_bytes() for name in BADGES}
		# Recheck after counting, which may take time on a large repository.
		check_badge_worktree(root, entries)
		updates = []
		for name, content in generated.items():
			object_id = git(root, 'hash-object', '-w', '--stdin', input=content).strip().decode('ascii')
			mode = entries.get(name, ('100644', ''))[0]
			updates.append(f'{mode} {object_id}\t{name.as_posix()}\0'.encode())
		git(root, 'update-index', '-z', '--index-info', input=b''.join(updates))
		for name, content in generated.items():
			path = root / name
			path.parent.mkdir(parents=True, exist_ok=True)
			if not path.exists() or path.read_bytes() != content:
				path.write_bytes(content)


def main():
	'''Report actionable hook failures without silently accepting stale badges.'''
	try:
		root = Path(os.fsdecode(git(Path.cwd(), 'rev-parse', '--show-toplevel')).strip())
		print('Updating code-count badges from staged files...', file=sys.stderr)
		refresh(root)
	except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
		detail = error.stderr.decode(errors='replace').strip() if isinstance(error, subprocess.CalledProcessError) and error.stderr else str(error)
		print(f'Youta badge hook: {detail}', file=sys.stderr)
		return 1
	return 0


if __name__ == '__main__':
	sys.exit(main())
