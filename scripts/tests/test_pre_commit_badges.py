'''Exercise automatic badges against small real Git indexes without network access.'''

import importlib.util
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPTS = Path(__file__).resolve().parents[1]


def load_script(name):
	'''Load a repository script without requiring an installed Python package.'''
	spec = importlib.util.spec_from_file_location(name, SCRIPTS / f'{name}.py')
	module = importlib.util.module_from_spec(spec)
	spec.loader.exec_module(module)
	return module


badges = load_script('pre_commit_badges')
installer = load_script('install_git_hooks')


class BadgeHookTests(unittest.TestCase):
	'''Keep staged source, generated SVGs, and working-tree edits distinguishable.'''

	def setUp(self):
		'''Build an unborn fixture repository with the real staged counter.'''
		clean_environment = {key: value for key, value in os.environ.items() if not key.startswith('GIT_')}
		self.environment = mock.patch.dict(os.environ, clean_environment, clear=True)
		self.environment.start()
		self.addCleanup(self.environment.stop)
		self.temporary = tempfile.TemporaryDirectory()
		self.addCleanup(self.temporary.cleanup)
		self.root = Path(self.temporary.name)
		self.git('init', '-q')
		self.git('config', 'user.name', 'Badge test')
		self.git('config', 'user.email', 'badge-test@example.invalid')
		self.git('config', 'core.hooksPath', '.git/hooks')
		for name in ('production_loc.py', 'production-loc-requirements.txt', 'pre_commit_badges.py'):
			self.write(f'scripts/{name}', (SCRIPTS / name).read_text(encoding='utf-8'))
		self.write('src/lib.rs', 'pub fn first() {}\n')
		self.git('add', '.')

	def git(self, *arguments, env=None):
		'''Execute Git in the fixture and retain raw index/blob bytes.'''
		return subprocess.check_output(['git', *arguments], cwd=self.root, env=env, stderr=subprocess.PIPE)

	def write(self, name, content):
		'''Create fixture content with the repository newline convention.'''
		path = self.root / name
		path.parent.mkdir(parents=True, exist_ok=True)
		path.write_text(content, encoding='utf-8')

	def commit(self, *arguments):
		'''Create only temporary fixture commits with the required attribution.'''
		return self.git('commit', '-q', *arguments, '-m', 'Fixture commit\n\nCo-authored-by: OpenAI ChatGPT <noreply@openai.com>')

	def install_fixture_hook(self):
		'''Run the hook with this test interpreter, avoiding dependency installation.'''
		self.write('.git/hooks/pre-commit', '#!/bin/sh\nexec ' + shlex.quote(sys.executable) + ' scripts/pre_commit_badges.py\n')
		(self.root / '.git/hooks/pre-commit').chmod(0o755)

	def assert_production_count(self, count, revision=':'):
		'''Read the actual staged or committed badge, never the working copy.'''
		content = self.git('show', f'{revision}docs/badges/production-code.svg').decode()
		self.assertIn(f'production code: {count:,} lines', content)

	def test_initial_commit_counts_staged_source_and_preserves_unstaged_edits(self):
		'''A partially staged file must not leak its working changes into the count.'''
		unstaged = 'pub fn first() {}\npub fn unstaged() {}\n'
		self.write('src/lib.rs', unstaged)
		badges.refresh(self.root)
		self.assert_production_count(1)
		self.assertEqual((self.root / 'src/lib.rs').read_text(), unstaged)
		self.assertEqual(self.git('show', ':src/lib.rs'), b'pub fn first() {}\n')
		for name in badges.BADGES:
			self.assertEqual(self.git('show', f':{name}'), (self.root / name).read_bytes())

	def test_staged_additions_deletions_and_renames_define_the_snapshot(self):
		'''Module discovery follows the new index rather than deleted working files.'''
		self.write('src/lib.rs', 'mod old;\npub fn first() {}\n')
		self.write('src/old.rs', 'pub fn helper() {}\n')
		self.git('add', 'src')
		badges.refresh(self.root)
		self.commit()
		self.git('mv', 'src/old.rs', 'src/new.rs')
		self.write('src/lib.rs', 'mod new;\npub fn first() {}\n')
		self.git('add', 'src/lib.rs')
		self.write('src/new.rs', 'invalid unstaged Rust')
		badges.refresh(self.root)
		self.assert_production_count(3)
		self.assertEqual((self.root / 'src/new.rs').read_text(), 'invalid unstaged Rust')

	def test_unstaged_badge_edits_abort_without_changing_the_index(self):
		'''Generated-file automation must not overwrite a user's pending badge edit.'''
		badges.refresh(self.root)
		self.write(str(badges.BADGES[0]), 'manual badge change\n')
		before = self.git('write-tree')
		with self.assertRaisesRegex(RuntimeError, 'unstaged badge'):
			badges.refresh(self.root)
		self.assertEqual(self.git('write-tree'), before)
		self.assertEqual((self.root / badges.BADGES[0]).read_text(), 'manual badge change\n')

	def test_commit_all_updates_badges_in_the_same_commit(self):
		'''Git's temporary index for commit -a must receive the generated blobs.'''
		badges.refresh(self.root)
		self.commit()
		self.install_fixture_hook()
		self.write('src/lib.rs', 'pub fn first() {}\npub fn second() {}\n')
		self.commit('-a')
		self.assert_production_count(2, 'HEAD:')
		self.assertEqual(self.git('status', '--porcelain'), b'')

	def test_alternate_index_does_not_modify_the_default_index(self):
		'''Every Git subprocess must retain the inherited index selection.'''
		badges.refresh(self.root)
		self.commit()
		original = (self.root / '.git/index').read_bytes()
		alternate = self.root / '.git/alternate-index'
		shutil.copyfile(self.root / '.git/index', alternate)
		with mock.patch.dict(os.environ, {'GIT_INDEX_FILE': str(alternate)}):
			self.write('src/lib.rs', 'pub fn first() {}\npub fn second() {}\n')
			self.git('add', 'src/lib.rs')
			badges.refresh(self.root)
			self.assert_production_count(2)
		self.assertEqual((self.root / '.git/index').read_bytes(), original)

	def test_staged_counter_is_used_even_when_working_counter_is_invalid(self):
		'''An unstaged counter edit cannot change or break the committed counts.'''
		self.write('scripts/production_loc.py', 'invalid Python syntax !')
		badges.refresh(self.root)
		self.assert_production_count(1)

	def test_export_uses_raw_blobs_without_running_smudge_filters(self):
		'''Checkout filters must not change counts or perform network work in hooks.'''
		self.write('.gitattributes', '*.rs filter=addline\n')
		self.git('config', 'filter.addline.clean', 'cat')
		command = 'import sys; sys.stdout.write(sys.stdin.read() + "pub fn smudged() {}\\n")'
		self.git('config', 'filter.addline.smudge', shlex.quote(sys.executable) + ' -c ' + shlex.quote(command))
		self.git('add', '.gitattributes')
		badges.refresh(self.root)
		self.assert_production_count(1)

	def test_git_reads_disable_lazy_fetch_without_losing_the_active_index(self):
		'''A partial clone must fail locally instead of fetching blobs in a hook.'''
		with mock.patch.dict(os.environ, {'GIT_INDEX_FILE': 'fixture-index', 'GIT_NO_LAZY_FETCH': '0'}):
			with mock.patch.object(badges.subprocess, 'check_output', return_value=b'blob') as command:
				self.assertEqual(badges.git(self.root, 'cat-file', '--batch', input=b'object\n'), b'blob')
		environment = command.call_args.kwargs['env']
		self.assertEqual(environment['GIT_NO_LAZY_FETCH'], '1')
		self.assertEqual(environment['GIT_INDEX_FILE'], 'fixture-index')

	def test_staged_badge_deletion_is_regenerated(self):
		'''All three required badges remain present even after an indexed deletion.'''
		badges.refresh(self.root)
		self.commit()
		self.git('rm', str(badges.BADGES[0]))
		badges.refresh(self.root)
		self.assert_production_count(1)
		self.assertEqual(self.git('status', '--porcelain'), b'')

	def test_missing_pinned_dependency_aborts_before_mutation(self):
		'''Dependency drift needs explicit setup, never network access in a hook.'''
		before = self.git('write-tree')
		with mock.patch.object(badges.metadata, 'version', return_value='0'):
			with self.assertRaisesRegex(RuntimeError, 'install_git_hooks.py'):
				badges.refresh(self.root)
		self.assertEqual(self.git('write-tree'), before)
		self.assertFalse((self.root / badges.BADGES[0]).exists())

	def test_snapshot_failure_leaves_existing_badges_and_index_unchanged(self):
		'''Invalid staged source cannot partially regenerate the output files.'''
		badges.refresh(self.root)
		self.write('src/lib.rs', 'invalid Rust syntax !')
		self.git('add', 'src/lib.rs')
		before = self.git('write-tree')
		original = {name: (self.root / name).read_bytes() for name in badges.BADGES}
		with self.assertRaises(subprocess.CalledProcessError):
			badges.refresh(self.root)
		self.assertEqual(self.git('write-tree'), before)
		self.assertEqual({name: (self.root / name).read_bytes() for name in badges.BADGES}, original)

	def test_installer_is_idempotent_and_uses_the_effective_hook_path(self):
		'''Installing never replaces hooksPath or requires network in this test.'''
		self.git('config', 'core.hooksPath', '.custom-hooks')
		with mock.patch.object(installer, 'prepare_environment') as prepare:
			installer.install(self.root)
			installer.install(self.root)
		self.assertEqual(prepare.call_count, 2)
		hook = self.root / '.custom-hooks/pre-commit'
		self.assertIn(installer.MARKER, hook.read_text())
		self.assertTrue(os.access(hook, os.X_OK))
		self.assertEqual(self.git('config', '--get', 'core.hooksPath'), b'.custom-hooks\n')

	def test_installer_refuses_existing_hooks_before_installing_dependencies(self):
		'''Another hook's content must survive setup without alteration.'''
		self.write('.git/hooks/pre-commit', '#!/bin/sh\nprintf existing\n')
		with mock.patch.object(installer, 'prepare_environment') as prepare:
			with self.assertRaisesRegex(RuntimeError, 'existing'):
				installer.install(self.root)
		prepare.assert_not_called()
		self.assertEqual((self.root / '.git/hooks/pre-commit').read_text(), '#!/bin/sh\nprintf existing\n')

	def test_installer_refuses_shared_external_hook_directories(self):
		'''A global hooksPath must never receive a repository-specific launcher.'''
		with tempfile.TemporaryDirectory() as shared:
			self.git('config', 'core.hooksPath', shared)
			with mock.patch.object(installer, 'prepare_environment') as prepare:
				with self.assertRaisesRegex(RuntimeError, 'outside this repository'):
					installer.install(self.root)
			prepare.assert_not_called()
			self.assertFalse((Path(shared) / 'pre-commit').exists())


if __name__ == '__main__':
	unittest.main()
