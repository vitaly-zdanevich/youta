#!/usr/bin/env python3
'''Reproduce the README production/test code badges without compiling dependencies.

Follow Rust modules from the application/build entry points, masking syntax
that cannot exist with cfg(test) disabled. Other features/platforms remain
unknown, so optional production implementations are included. Pygments counts
nonblank, noncomment physical lines in the remaining Rust and frontend code.
See docs/PRODUCTION_CODE.md for the precise scope and regeneration command.
'''

import argparse
import json
from pathlib import Path, PurePosixPath
import posixpath
import re
import subprocess

from pygments import lex
from pygments.lexers import get_lexer_for_filename
from pygments.token import Comment, String
from tree_sitter import Language, Parser
import tree_sitter_rust


ROOT = Path(__file__).resolve().parents[1]
BADGE = Path('docs/badges/production-code.svg')
TEST_BADGE = Path('docs/badges/test-code.svg')
RUST_ROOTS = ('src/lib.rs', 'src/main.rs', 'build.rs', 'gui/src/main.rs', 'gui/build.rs')
COMMENTS = {'line_comment', 'block_comment'}


def groups(node):
	'''Split a syntax token tree at its own commas, never inside nested trees.'''
	result = [[]]
	for child in node.children[1:-1]:
		if child.type == ',':
			result.append([])
		elif child.type not in COMMENTS:
			result[-1].append(child)
	return [group for group in result if group]


def cfg_value(tokens, test=False):
	'''Evaluate a test configuration, retaining unknown features/platforms.'''
	if len(tokens) == 1:
		return {b'test': test, b'false': False, b'true': True}.get(tokens[0].text)
	if len(tokens) != 2 or tokens[1].type != 'token_tree':
		return None
	name = tokens[0].text
	values = [cfg_value(group, test) for group in groups(tokens[1])]
	if name == b'all':
		return False if False in values else (None if None in values else True)
	if name == b'any':
		return True if True in values else (None if None in values else False)
	if name == b'not' and len(values) == 1:
		return None if values[0] is None else not values[0]
	return None


def enabled(tokens, test=False):
	'''Recognize cfg/test attributes, including nested conditional attributes.'''
	name = tokens[0].text
	if name == b'test' or name.endswith(b'::test'):
		return test
	if len(tokens) != 2 or tokens[1].type != 'token_tree':
		return True
	arguments = groups(tokens[1])
	if name == b'cfg':
		return cfg_value(arguments[0], test) is not False
	if name == b'cfg_attr' and cfg_value(arguments[0], test) is True:
		return all(enabled(argument, test) for argument in arguments[1:])
	return True


def attribute_tokens(node):
	'''Read an attribute's syntax without interpreting strings as Rust code.'''
	return [child for child in node.named_children[0].children if child.type not in COMMENTS]


def conditional_path(tokens, test=False):
	'''Reject unresolved conditional module paths instead of counting a wrong file.'''
	if tokens[0].text == b'path':
		return True
	if tokens[0].text == b'cfg_attr':
		arguments = groups(tokens[1])
		return cfg_value(arguments[0], test) is not False and any(conditional_path(arg, test) for arg in arguments[1:])
	return False


def code_line_numbers(source, filename):
	'''Locate physical code lines so production and tests never count one twice.'''
	lexer = get_lexer_for_filename(filename, stripnl=False, ensurenl=False)
	parts = []
	for kind, value in lex(source, lexer):
		# Preprocessor-like tokens are code, unlike ordinary documentation/comments.
		documentation = (kind in Comment and kind not in Comment.Preproc) or kind in String.Doc
		parts.append(re.sub(r'[^\n]', ' ', value) if documentation else value)
	return {index for index, line in enumerate(''.join(parts).split('\n')) if line.strip()}


def code_lines(source, filename):
	'''Count physical lines containing code; strings and mixed comments stay code.'''
	return len(code_line_numbers(source, filename))


def rust_source(source, path, tracked, test=False, *, crate_root=False):
	'''Mask disabled syntax and return module paths for the chosen test setting.'''
	parser = Parser(Language(tree_sitter_rust.language()))
	tree = parser.parse(source)
	if tree.root_node.has_error:
		raise ValueError(f'Cannot parse Rust source: {path}')
	masked = bytearray(source)
	modules = []

	def mask(start, end):
		'''Preserve line breaks and all neighbouring production tokens.'''
		masked[start:end] = re.sub(rb'[^\n]', b' ', source[start:end])

	def walk(parent, module_dir, path_dir):
		'''Visit attributes at every syntax level, not only top-level modules.'''
		if parent.type in {'field_initializer', 'shorthand_field_initializer', 'match_arm'}:
			owned = [child for child in parent.named_children if child.type == 'attribute_item']
			if not all(enabled(attribute_tokens(attribute), test) for attribute in owned):
				end = parent.end_byte
				if parent.next_sibling is not None and parent.next_sibling.type == ',':
					end = parent.next_sibling.end_byte
				mask(parent.start_byte, end)
				return
		attributes = []
		for node in parent.named_children:
			if node.type in COMMENTS:
				continue
			if node.type == 'inner_attribute_item':
				if not enabled(attribute_tokens(node), test):
					mask(parent.start_byte, parent.end_byte)
					return
				continue
			if node.type == 'attribute_item':
				attributes.append(node)
				continue
			if not all(enabled(attribute_tokens(attribute), test) for attribute in attributes):
				end = node.end_byte
				if node.next_sibling is not None and node.next_sibling.type == ',':
					end = node.next_sibling.end_byte
				mask(attributes[0].start_byte, end)
				attributes = []
				continue
			for attribute in attributes:
				tokens = attribute_tokens(attribute)
				if tokens[0].text == b'cfg_attr' and cfg_value(groups(tokens[1])[0], test) is False:
					mask(attribute.start_byte, attribute.end_byte)
			if node.type == 'mod_item':
				name = node.child_by_field_name('name').text.decode()
				body = node.child_by_field_name('body')
				for attribute in attributes:
					tokens = attribute_tokens(attribute)
					if (tokens[0].text == b'cfg_attr' or body is not None) and conditional_path(tokens, test):
						raise ValueError(f'Unsupported conditional/inline module path in {path}: {name}')
				if body is not None:
					walk(body, module_dir / name, module_dir / name)
				else:
					candidates = [module_dir / f'{name}.rs', module_dir / name / 'mod.rs']
					for attribute in attributes:
						value = attribute.named_children[0]
						if value.named_children[0].text == b'path':
							candidates = [path_dir / json.loads(value.child_by_field_name('value').text)]
					candidates = [PurePosixPath(posixpath.normpath(str(candidate))) for candidate in candidates]
					found = [candidate for candidate in candidates if str(candidate) in tracked]
					if len(found) != 1:
						raise ValueError(f'Expected one tracked module for {path}: {name}')
					modules.extend(found)
			elif node.type not in {'string_literal', 'raw_string_literal', 'token_tree'}:
				walk(node, module_dir, path_dir)
			attributes = []

	module_dir = path.parent if crate_root or path.name in {'lib.rs', 'main.rs', 'mod.rs', 'build.rs'} else path.with_suffix('')
	walk(tree.root_node, module_dir, path.parent)
	return masked.decode(), modules


def rust_sources(root, tracked, test=False):
	'''Follow owned Rust roots once, including integration roots in test mode.'''
	sources = {}
	roots = {path for path in RUST_ROOTS if path in tracked}
	if test:
		roots.update(name for name in tracked if re.fullmatch(r'tests/[^/]+(?:\.rs|/main\.rs)', name))
	queue = [PurePosixPath(name) for name in sorted(roots)]
	while queue:
		path = queue.pop()
		name = str(path)
		if name in sources or path.stem.endswith('_generated'):
			continue
		allowed = ('src/', 'gui/src/', 'tests/') if test else ('src/', 'gui/src/')
		if name not in RUST_ROOTS and not name.startswith(allowed):
			raise ValueError(f'Rust module outside owned roots: {path}')
		if '..' in path.parts or (root / path).is_symlink():
			raise ValueError(f'Unexpected source path: {path}')
		source, modules = rust_source((root / path).read_bytes(), path, tracked, test, crate_root=name in roots)
		sources[name] = source
		queue.extend(modules)
	return sources


def production_counts(root, tracked):
	'''Count only owned application/build sources, never installed dependencies.'''
	counts = {name: code_lines(source, name) for name, source in rust_sources(root, tracked).items()}
	for name in sorted(tracked):
		path = PurePosixPath(name)
		frontend = name.startswith('gui/ui/src/') or name in {'gui/ui/index.html', 'gui/ui/vite.config.ts'}
		embedded_lua = name.startswith('src/') and path.suffix == '.lua'
		if not embedded_lua and (not frontend or path.suffix not in {'.ts', '.tsx', '.js', '.mjs', '.css', '.html'}):
			continue
		if set(path.parts) & {'tests', '__tests__', 'fixtures', 'generated', 'vendor', 'node_modules'} or re.search(r'\.(test|spec)\.', path.name) or path.stem.endswith('_generated'):
			continue
		if '..' in path.parts or (root / path).is_symlink():
			raise ValueError(f'Unexpected source path: {path}')
		counts[name] = code_lines((root / path).read_text(encoding='utf-8'), name)
	return dict(sorted(counts.items()))


def test_counts(root, tracked):
	'''Count test-only Rust lines and owned test programs, not shared runtime code.'''
	production = rust_sources(root, tracked)
	counts = {}
	for name, source in rust_sources(root, tracked, test=True).items():
		lines = code_line_numbers(source, name) - code_line_numbers(production.get(name, ''), name)
		if lines:
			counts[name] = len(lines)
	for name in sorted(tracked):
		path = PurePosixPath(name)
		in_tests = name.startswith(('tests/', 'scripts/tests/', 'gui/ui/tests/'))
		frontend = name.startswith('gui/ui/src/') and (
			bool(set(path.parts) & {'tests', '__tests__'}) or bool(re.search(r'\.(test|spec)\.', path.name))
		)
		if not (in_tests or frontend) or path.suffix not in {'.py', '.ts', '.tsx', '.js', '.mjs', '.lua', '.sh'}:
			continue
		if set(path.parts) & {'generated', 'vendor', 'node_modules'} or path.stem.endswith('_generated'):
			continue
		if '..' in path.parts or (root / path).is_symlink():
			raise ValueError(f'Unexpected source path: {path}')
		counts[name] = code_lines((root / path).read_text(encoding='utf-8'), name)
	return dict(sorted(counts.items()))


def badge_svg(count, *, tests=False):
	'''Render a deterministic local badge with an accessible exact count.'''
	value = f'{count:,}'
	label = 'test code' if tests else 'production code'
	excluded = 'generated data and dependencies' if tests else 'tests, generated data and dependencies'
	return f'''<svg xmlns="http://www.w3.org/2000/svg" width="190" height="20" role="img" aria-label="{label}: {value} lines">
	<title>{label.capitalize()}: {value} lines; excludes {excluded}</title>
	<linearGradient id="shade" x2="0" y2="100%"><stop offset="0" stop-color="#fff" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
	<clipPath id="round"><rect width="190" height="20" rx="3"/></clipPath>
	<g clip-path="url(#round)"><path fill="#555" d="M0 0h115v20H0z"/><path fill="#007ec6" d="M115 0h75v20h-75z"/><path fill="url(#shade)" d="M0 0h190v20H0z"/></g>
	<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11">
		<text x="57.5" y="15" fill="#010101" fill-opacity=".3">{label}</text><text x="57.5" y="14">{label}</text>
		<text x="152.5" y="15" fill="#010101" fill-opacity=".3">{value}</text><text x="152.5" y="14">{value}</text>
	</g>
</svg>
'''


def main():
	'''Print per-file counts, regenerate both SVGs, or reject either stale badge.'''
	parser = argparse.ArgumentParser(description=__doc__)
	mode = parser.add_mutually_exclusive_group()
	mode.add_argument('--write', action='store_true', help='regenerate both checked-in SVGs')
	mode.add_argument('--check', action='store_true', help='fail if either checked-in SVG is stale')
	args = parser.parse_args()
	tracked = set(subprocess.check_output(['git', 'ls-files', '-z'], cwd=ROOT).decode().split('\0')) - {''}
	counts = production_counts(ROOT, tracked)
	tests = test_counts(ROOT, tracked)
	total = sum(counts.values())
	print(json.dumps({'total': total, 'files': counts, 'tests': {'total': sum(tests.values()), 'files': tests}}, indent=2))
	for badge, svg in [(BADGE, badge_svg(total)), (TEST_BADGE, badge_svg(sum(tests.values()), tests=True))]:
		if args.write:
			(ROOT / badge).parent.mkdir(parents=True, exist_ok=True)
			(ROOT / badge).write_text(svg, encoding='utf-8')
		elif args.check and (not (ROOT / badge).exists() or (ROOT / badge).read_text(encoding='utf-8') != svg):
			parser.exit(1, f'{badge} is stale. Run: python scripts/production_loc.py --write\n')


if __name__ == '__main__':
	main()
