'''Offline contracts for the first-party production-code README badge.'''

import importlib.util
from contextlib import redirect_stderr, redirect_stdout
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock
import xml.etree.ElementTree as ET


SPEC = importlib.util.spec_from_file_location('production_loc', Path(__file__).parents[1] / 'production_loc.py')
production_loc = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = production_loc
SPEC.loader.exec_module(production_loc)


class ProductionLocTests(unittest.TestCase):
	'''Use miniature repositories to separate runtime code from test scaffolding.'''

	def count_fixture(self, files, untracked=None):
		'''Count tracked fixture files without reading the real working tree.'''
		with tempfile.TemporaryDirectory() as temporary:
			root = Path(temporary)
			for name, content in {**files, **(untracked or {})}.items():
				path = root / name
				path.parent.mkdir(parents=True, exist_ok=True)
				path.write_text(content, encoding='utf-8')
			return production_loc.production_counts(root, set(files))

	def test_rust_modules_are_reachable_once_from_runtime_and_build_roots(self):
		'''Shared modules count once; orphan helpers and integration tests do not.'''
		counts = self.count_fixture({
			'src/lib.rs': 'mod shared;\npub fn library() {}\n',
			'src/main.rs': 'mod shared;\nfn main() {}\n',
			'src/shared.rs': 'pub fn shared() {}\n',
			'src/orphan.rs': 'pub fn unreachable() {}\n',
			'build.rs': 'fn main() {}\n',
			'gui/src/main.rs': 'fn main() {}\n',
			'gui/build.rs': 'fn main() {}\n',
			'tests/integration.rs': '#[test]\nfn integration() {}\n',
			'examples/demo.rs': 'fn main() {}\n',
		})
		self.assertEqual(counts, {
			'src/lib.rs': 2,
			'src/main.rs': 2,
			'src/shared.rs': 1,
			'build.rs': 1,
			'gui/src/main.rs': 1,
			'gui/build.rs': 1,
		})

	def test_inline_tests_do_not_count_as_production(self):
		'''An inline Rust test module must not inflate its containing file.'''
		counts = self.count_fixture({
			'src/lib.rs': (
				'pub fn production() {}\n'
				'#[cfg(test)]\n'
				'mod tests {\n'
				'\tuse super::*;\n'
				'\t#[test]\n'
				'\tfn production_works() { production(); }\n'
				'}\n'
			),
		})
		self.assertEqual(counts['src/lib.rs'], 1)

	def test_test_only_modules_with_neutral_names_and_path_attributes_are_excluded(self):
		'''Ownership, not a filename heuristic, identifies out-of-line tests.'''
		counts = self.count_fixture({
			'src/lib.rs': (
				'pub fn production() {}\n'
				'#[cfg(test)]\nmod performance;\n'
				'#[cfg(all(test, unix))]\n#[path = "fixtures/checks.rs"]\nmod checks;\n'
			),
			'src/performance.rs': 'pub fn fixture() {}\n',
			'src/fixtures/checks.rs': 'pub fn fixture() {}\n',
		})
		self.assertEqual(counts['src/lib.rs'], 1)
		self.assertNotIn('src/performance.rs', counts)
		self.assertNotIn('src/fixtures/checks.rs', counts)

	def test_cfg_keeps_non_test_platform_and_feature_branches(self):
		'''Unknown production configurations must not be treated as disabled.'''
		counts = self.count_fixture({
			'src/lib.rs': (
				'#[cfg(any(windows, test))]\nmod windows;\n'
				'#[cfg(any(feature = "radio", all(test, unix)))]\nmod radio;\n'
				'#[cfg(all(test, not(feature = "radio")))]\nmod checks;\n'
				'#[cfg(not(test))]\nmod release;\n'
			),
			'src/windows.rs': 'pub fn windows() {}\n',
			'src/radio.rs': 'pub fn radio() {}\n',
			'src/checks.rs': 'pub fn fixture() {}\n',
			'src/release.rs': 'pub fn release() {}\n',
		})
		self.assertEqual(counts['src/windows.rs'], 1)
		self.assertEqual(counts['src/radio.rs'], 1)
		self.assertEqual(counts['src/release.rs'], 1)
		self.assertNotIn('src/checks.rs', counts)

	def test_test_only_fields_initializers_methods_and_statements_are_removed(self):
		'''Tests outside a tests module cannot leak into the production count.'''
		counts = self.count_fixture({
			'src/lib.rs': (
				'struct Settings {\n'
				'\t#[cfg(test)]\n\tprobe: bool,\n'
				'\tvalue: u32,\n}\n'
				'impl Settings {\n'
				'\t#[cfg(all(test, unix))]\n\tfn fixture(&self) {}\n'
				'\tfn value(&self) -> u32 { self.value }\n}\n'
				'fn settings() -> Settings {\n'
				'\t#[cfg(test)]\n\tprintln!("test probe");\n'
				'\tSettings {\n'
				'\t\t#[cfg(test)]\n\t\tprobe: false,\n'
				'\t\tvalue: 1,\n\t}\n}\n'
			),
		})
		self.assertEqual(counts['src/lib.rs'], 11)

	def test_raw_strings_and_shared_lines_preserve_real_production_code(self):
		'''Fake attributes in strings are data, and a shared line counts once.'''
		counts = self.count_fixture({
			'src/lib.rs': (
				'const TEXT: &str = r###"\n'
				'#[cfg(test)]\nmod tests { }\n// not a comment\n"###;\n'
				'fn production() {} #[cfg(test)] mod tests { fn fixture() {} }\n'
			),
		})
		self.assertEqual(counts['src/lib.rs'], 6)

	def test_inner_cfg_direct_tests_and_conditional_test_attributes_are_excluded(self):
		'''Test annotations remain exclusions even without a conventional module.'''
		counts = self.count_fixture({
			'src/lib.rs': (
				'pub fn production() {}\n'
				'#[test]\nfn standalone_test() {}\n'
				'#[cfg_attr(not(test), cfg(test))]\nfn disabled() {}\n'
				'#[cfg_attr(test, allow(dead_code))]\nfn runtime() {}\n'
				'mod support;\n'
			),
			'src/support.rs': '#![cfg(test)]\nfn fixture() {}\n',
		})
		self.assertEqual(counts['src/lib.rs'], 3)
		self.assertEqual(counts.get('src/support.rs', 0), 0)

	def test_test_only_match_arms_and_unicode_do_not_shift_mask_boundaries(self):
		'''UTF-8 byte ranges must not corrupt surrounding runtime statements.'''
		counts = self.count_fixture({
			'src/lib.rs': (
				'const TITLE: &str = "Музыка";\n'
				'fn choose(value: u32) -> u32 {\n'
				'\tmatch value {\n'
				'\t\t#[cfg(test)]\n\t\t0 => 0,\n'
				'\t\t_ => 1,\n\t}\n}\n'
			),
		})
		self.assertEqual(counts['src/lib.rs'], 6)

	def test_rust_nested_and_explicit_path_modules_are_followed(self):
		'''Runtime module resolution supports both conventional Rust layouts.'''
		counts = self.count_fixture({
			'src/lib.rs': 'mod folder;\n#[path = "elsewhere/selected.rs"]\nmod selected;\n',
			'src/folder/mod.rs': 'mod child;\n',
			'src/folder/child.rs': 'pub fn child() {}\n',
			'src/elsewhere/selected.rs': 'pub fn selected() {}\n',
		})
		self.assertEqual(counts['src/folder/child.rs'], 1)
		self.assertEqual(counts['src/elsewhere/selected.rs'], 1)

	def test_conditional_or_inline_path_overrides_fail_instead_of_counting_wrong_module(self):
		'''Unsupported overrides must not silently select a conventional filename.'''
		for predicate in ['not(test)', 'unix']:
			with self.subTest(predicate=predicate):
				with self.assertRaises(ValueError):
					self.count_fixture({
						'src/lib.rs': f'#[cfg_attr({predicate}, path = "real.rs")]\nmod thing;\n',
						'src/thing.rs': 'pub fn wrong() {}\n',
						'src/real.rs': 'pub fn selected() {}\n',
					})
		with self.assertRaises(ValueError):
			self.count_fixture({
				'src/lib.rs': '#[path = "custom"]\nmod inline { mod child; }\n',
				'src/inline/child.rs': 'pub fn wrong() {}\n',
				'src/custom/child.rs': 'pub fn selected() {}\n',
			})

	def test_test_only_conditional_path_override_does_not_change_runtime_module(self):
		'''A known-false conditional path attribute has no production effect.'''
		counts = self.count_fixture({
			'src/lib.rs': '#[cfg_attr(test, path = "fixture.rs")]\nmod runtime;\n',
			'src/runtime.rs': 'pub fn production() {}\n',
			'src/fixture.rs': 'pub fn fixture() {}\n',
		})
		self.assertEqual(counts, {'src/lib.rs': 1, 'src/runtime.rs': 1})

	def test_generated_code_dependencies_and_untracked_files_are_excluded(self):
		'''A source allowlist excludes data snapshots and dependency/build trees.'''
		counts = self.count_fixture({
			'src/lib.rs': 'mod stations_generated;\npub fn production() {}\n',
			'src/stations_generated.rs': 'pub const STATIONS: &[u8] = &[1, 2, 3];\n',
			'src/providers/npr_station_quality_generated.json': '{"stations": []}\n',
			'gui/gen/schemas/desktop-schema.json': '{"generated": true}\n',
			'vendor/dependency/src/lib.rs': 'pub fn dependency() {}\n',
			'target/generated.rs': 'pub fn generated() {}\n',
			'gui/ui/node_modules/dependency/index.js': 'export const dependency = 1;\n',
			'gui/ui/dist/index.js': 'const bundled = 1;\n',
		}, untracked={'gui/ui/src/scratch.ts': 'export const scratch = 1;\n'})
		self.assertEqual(set(counts), {'src/lib.rs'})

	def test_frontend_keeps_runtime_sources_but_excludes_tests_and_generated_files(self):
		'''Frontend code belongs in the badge, while its test harness does not.'''
		files = {
			'gui/ui/src/App.tsx': 'export const App = () => <main />;\n',
			'gui/ui/src/actions.ts': 'export const action = 1;\n',
			'gui/ui/src/helpers.js': 'export const helper = 1;\n',
			'gui/ui/src/app.css': 'main { color: black; }\n',
			'gui/ui/src/fragment.html': '<main></main>\n',
			'gui/ui/index.html': '<html></html>\n',
			'gui/ui/vite.config.ts': 'export default {};\n',
			'gui/ui/src/actions.test.mjs': 'test("fixture", () => {});\n',
			'gui/ui/src/actions.spec.ts': 'test("fixture", () => {});\n',
			'gui/ui/src/__tests__/checks.ts': 'test("fixture", () => {});\n',
			'gui/ui/tests/browser.test.mjs': 'test("fixture", () => {});\n',
			'gui/ui/src/contracts_generated.ts': 'export const generated = 1;\n',
		}
		counts = self.count_fixture(files)
		self.assertEqual(counts, {name: 1 for name in list(files)[:7]})

	def test_code_lines_excludes_comments_but_keeps_strings_and_mixed_lines(self):
		'''Language-aware lexing must distinguish comments from literal text.'''
		for filename, source, expected in [
			('fixture.rs', '\n// comment\n/* outer /* nested */ comment */\nfn run() {} // mixed\n', 1),
			('fixture.rs', 'const TEXT: &str = "// literal";\n', 1),
			('fixture.ts', '// comment\nconst text = "/* literal */";\n/* comment */ const value = 1;\n', 2),
			('fixture.css', '/* comment */\nbody {\n\tcolor: black; /* mixed */\n}\n', 3),
			('fixture.html', '<!-- comment -->\n<main>literal</main>\n', 1),
		]:
			with self.subTest(filename=filename, source=source):
				self.assertEqual(production_loc.code_lines(source, filename), expected)

	def test_badge_is_deterministic_self_contained_and_displays_the_count(self):
		'''Rendering a count must produce a valid SVG without external assets.'''
		badge = production_loc.badge_svg(12345)
		self.assertEqual(badge, production_loc.badge_svg(12345))
		root = ET.fromstring(badge)
		self.assertEqual(root.tag, '{http://www.w3.org/2000/svg}svg')
		self.assertIn('12345', ''.join(root.itertext()).replace(',', '').replace(' ', ''))
		self.assertNotIn('<script', badge)
		self.assertNotIn('<image', badge)
		self.assertNotIn('href=', badge)

	def test_malformed_rust_or_unresolved_runtime_module_fails_instead_of_undercounting(self):
		'''An incomplete analysis must never silently publish a smaller number.'''
		for source in ['fn broken( {\n', 'mod missing;\n']:
			with self.subTest(source=source):
				with self.assertRaises(ValueError):
					self.count_fixture({'src/lib.rs': source})

	def test_cli_rejects_missing_and_stale_badges_and_regenerates_exact_svg(self):
		'''The documented write/check flow guards the checked-in badge value.'''
		with tempfile.TemporaryDirectory() as temporary:
			root = Path(temporary)
			(root / 'src').mkdir()
			source = root / 'src/lib.rs'
			source.write_text('pub fn production() {}\n', encoding='utf-8')
			badge = root / production_loc.BADGE

			def invoke(mode):
				'''Run the actual command entry point against an isolated repository.'''
				with mock.patch.object(production_loc, 'ROOT', root):
					with mock.patch.object(production_loc.subprocess, 'check_output', return_value=b'src/lib.rs\0'):
						with mock.patch.object(sys, 'argv', ['production_loc.py', mode]):
							with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
								production_loc.main()

			with self.assertRaises(SystemExit) as missing:
				invoke('--check')
			self.assertEqual(missing.exception.code, 1)
			invoke('--write')
			self.assertEqual(badge.read_text(encoding='utf-8'), production_loc.badge_svg(1))
			invoke('--check')
			source.write_text('pub fn production() {}\npub fn additional() {}\n', encoding='utf-8')
			with self.assertRaises(SystemExit) as stale:
				invoke('--check')
			self.assertEqual(stale.exception.code, 1)
			invoke('--write')
			self.assertEqual(badge.read_text(encoding='utf-8'), production_loc.badge_svg(2))
			invoke('--check')

	def test_readme_places_production_badge_immediately_before_existing_loc_badge(self):
		'''The new badge stays to the left of the existing Sonar LOC badge.'''
		root = Path(__file__).parents[2]
		lines = (root / 'README.md').read_text(encoding='utf-8').splitlines()
		position = next(index for index, line in enumerate(lines) if 'metric=ncloc' in line)
		self.assertGreater(position, 0)
		self.assertIn('docs/badges/production-code.svg', lines[position - 1])
		self.assertIn('docs/PRODUCTION_CODE.md', lines[position - 1])
		self.assertTrue((root / 'docs/badges/production-code.svg').is_file())
		self.assertTrue((root / 'docs/PRODUCTION_CODE.md').is_file())


if __name__ == '__main__':
	unittest.main()
