/** YouTube search controls render reducer state and dispatch shared actions without local filtering. */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const actions = [];
const source = await readFile(new URL('./components/SearchBar.tsx', import.meta.url), 'utf8');
const compiled = ts.transpileModule(source, {
	compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const module = { exports: {} };
const mockRequire = (name) => name === '../ipc' ? { dispatch: (action) => actions.push(action) } : require(name);
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Traverse actual JSX host elements without creating a browser or mocking rendering decisions. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Read text children exactly as displayed, preserving the query and control labels. */
function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	return React.Children.toArray(element.props.children).map(text).join('');
}

const render = (patch = {}) => module.exports.SearchBar({ label: 'YouTube', verb: 'Search', view: {
	screen: 'Search', search_query: '', search_cursor_byte: 0, search_editing: false,
	subscriptions: { show_youtube_shorts: false }, ...patch,
} });
const shortsButton = (tree) => nodes(tree).find((node) => node.type === 'button' && text(node).includes('Shorts:'));

test('YouTube search exposes the saved Shorts state through its existing shared toggle action', () => {
	for (const enabled of [false, true]) {
		actions.length = 0;
		const button = shortsButton(render({ subscriptions: { show_youtube_shorts: enabled } }));
		assert.ok(button, 'YouTube search needs a Shorts toggle');
		assert.equal(text(button), `[h] Shorts: ${enabled ? 'on' : 'off'}`);
		assert.equal(button.props['aria-pressed'], enabled);
		assert.equal(button.props.disabled, false);
		assert.deepEqual(actions, [], 'rendering cannot change the shared preference');
		button.props.onClick();
		assert.deepEqual(actions, ['ToggleSubscriptionShorts']);
	}
});

test('the Search Shorts control stays scoped to the YouTube tab', () => {
	for (const screen of ['YouTubeMusic', 'ArchiveOrg', 'SoundCloud', 'Web', 'Subscriptions']) {
		assert.equal(shortsButton(render({ screen })), undefined, screen);
	}
});

test('query editing disables the Shorts button and preserves h as query text', () => {
	actions.length = 0;
	const tree = render({ search_editing: true, search_query: 'h', search_cursor_byte: 1 });
	const button = shortsButton(tree);
	assert.ok(button);
	assert.equal(button.props.disabled, true);
	const editor = nodes(tree).find((node) => node.props.onMouseDown);
	assert.equal(text(editor), 'h');
	assert.deepEqual(actions, [], 'query snapshots do not toggle Shorts');
});
