/** URL metadata stays a lazy, reducer-owned supplement to untouched descriptions. */
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const modules = new Map();
const actions = [];

/** Exercise actual Details/Description JSX, replacing hooks, artwork and native IPC only. */
function load(url) {
	if (modules.has(url.href)) return modules.get(url.href);
	const compiled = ts.transpileModule(readFileSync(url, 'utf8'), {
		compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const module = { exports: {} };
	const mockRequire = (name) => {
		if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }) };
		if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
		if (name === './Artwork') return { Artwork: () => null };
		if (name.startsWith('.')) {
			const source = new URL(`${name}.ts`, url);
			return load(existsSync(source) ? source : new URL(`${name}.tsx`, url));
		}
		return require(name);
	};
	new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);
	modules.set(url.href, module.exports);
	return module.exports;
}

/** Flatten function children without replacing rendering decisions or button handlers. */
function nodes(element) {
	if (Array.isArray(element)) return element.flatMap(nodes);
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Literal text must survive even when an upstream response resembles markup. */
function text(element) {
	if (Array.isArray(element)) return element.map(text).join('');
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

const { Details } = load(new URL('./components/Details.tsx', import.meta.url));
const comment = 'Comment: Привет https://example.org/a?one=1&two=2\n<script>literal comment</script>';
const entries = [
	{ url: 'https://example.org/a?one=1&two=2', expanded: false, loading: false, lines: [] },
	{ url: 'https://other.example/music', expanded: false, loading: false, lines: [] },
];
const render = (url_info = entries, external_opener_available = true, kind = 'Local') => Details({ kind, view: {
	screen: kind === 'Video' ? 'Search' : 'Local', playlist_item: null, details_focused: false,
	external_opener_available, yandex_music_actions: {},
	details: { title: 'Recording', source: kind === 'Video' ? 'YouTube' : 'Local',
		media_id: kind === 'Video' ? { source: 'you-tube', external_id: 'video-id' } : null, channel_id: '', channel_name: '',
		playlist_names: [], dearrow_title: null, description: comment, timecodes: [], video_links: [],
		wikidata_entities: [], links: [], url_info, url_info_offset: 1 },
} });
const section = (tree) => nodes(tree).find((node) => node.props['aria-label'] === 'URL information');
const buttons = (tree) => nodes(section(tree)).filter((node) => node.type === 'button');

test('local URL information is collapsed and makes no requests until its explicit toggle is activated', () => {
	actions.length = 0;
	const tree = render();
	const controls = buttons(tree);
	assert.deepEqual(controls.map(text), [entries[0].url, 'Info', entries[1].url, 'Info']);
	assert.deepEqual(actions, [], 'rendering must not start lookup or open a browser');
	assert.equal(text(nodes(tree).find((node) => 'data-description' in node.props)), comment);
	assert.equal(controls[3].props['aria-expanded'], false);
	assert.equal(controls[3].props.onFocus, undefined, 'focus must not trigger lookup');
	assert.equal(controls[3].props.onMouseEnter, undefined, 'hover must not trigger lookup');
	let stopped = false;
	controls[3].props.onClick({ stopPropagation: () => { stopped = true; } });
	assert.equal(stopped, true, 'toggle dispatch must not bubble into unrelated Details focus actions');
	assert.deepEqual(actions, [{ ToggleUrlInfo: 1 }]);
});

test('expanded website and RDAP facts use muted escaped text without rewriting comments', () => {
	const lines = ['Title: <img src=x onerror=alert(1)>', 'Description: music & sound', 'Registered: 2001-01-01'];
	const tree = render([{ ...entries[0], expanded: true, lines }]);
	assert.equal(text(nodes(tree).find((node) => 'data-description' in node.props)), comment);
	assert.deepEqual(buttons(tree).map(text), [entries[0].url, 'Hide']);
	assert.equal(buttons(tree)[1].props['aria-expanded'], true);
	for (const line of lines) {
		const node = nodes(section(tree)).find((node) => node.type === 'p' && text(node) === line);
		assert.ok(node, `missing line: ${line}`);
		assert.match(node.props.className, /\btext-ink-faint\b/);
	}
	assert.ok(nodes(tree).every((node) => !node.props.dangerouslySetInnerHTML));
	assert.equal(nodes(tree).some((node) => ['script', 'img', 'iframe', 'a'].includes(node.type)), false);
	actions.length = 0;
	buttons(tree)[1].props.onClick({ stopPropagation() {} });
	assert.deepEqual(actions, [{ ToggleUrlInfo: 0 }]);
	assert.ok(text(section(render([{ ...entries[0], expanded: true, loading: true }]))).includes('Loading...'));
	assert.equal(text(section(render([{ ...entries[0], loading: true, lines }]))).includes(lines[0]), false);
});

test('opening a URL uses only its core index and needs an external opener; Info does not', () => {
	for (const available of [false, true]) {
		const [open, info] = buttons(render(entries, available));
		assert.equal(open.props.disabled, !available);
		assert.equal(Boolean(info.props.disabled), false);
		actions.length = 0;
		open.props.onClick({ stopPropagation() {} });
		assert.deepEqual(actions, available ? [{ OpenUrlInfo: 0 }] : []);
		info.props.onClick({ stopPropagation() {} });
		assert.deepEqual(actions.at(-1), { ToggleUrlInfo: 0 });
	}
});

test('Enter and Space activate URL-info buttons once without falling through to playback shortcuts', () => {
	for (const available of [false, true]) {
		for (const [index, button] of buttons(render(entries, available)).slice(0, 2).entries()) {
			for (const key of ['Enter', ' ']) {
				actions.length = 0;
				const events = [];
				const event = { key, preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') };
				button.props.onKeyDown(event);
				assert.deepEqual(events, ['prevent', 'stop']);
				assert.deepEqual(actions, index === 1 ? [{ ToggleUrlInfo: 0 }] : available ? [{ OpenUrlInfo: 0 }] : []);
				button.props.onKeyDown({ ...event, repeat: true });
				assert.ok(actions.length <= 1, 'a held key cannot toggle repeatedly');
			}
		}
	}
	const button = buttons(render())[1];
	actions.length = 0;
	button.props.onKeyDown({ key: 'Escape', preventDefault: () => assert.fail(), stopPropagation: () => assert.fail() });
	assert.deepEqual(actions, []);
});

test('YouTube descriptions expose the same on-demand URL information without changing their text', () => {
	actions.length = 0;
	const tree = render(entries, true, 'Video');
	const controls = buttons(tree);
	assert.deepEqual(controls.map(text), [entries[0].url, 'Info', entries[1].url, 'Info']);
	assert.equal(text(nodes(tree).find((node) => 'data-description' in node.props)), comment);
	assert.deepEqual(actions, [], 'YouTube rendering must not prefetch URL information');
	controls[1].props.onClick({ stopPropagation() {} });
	assert.deepEqual(actions, [{ ToggleUrlInfo: 0 }]);
});

test('providers with no URL metadata add no unused section', () => {
	for (const kind of ['Local', 'Video', 'Podcast', 'Radio', 'YandexMusic', 'Channel', 'Generic']) {
		assert.equal(section(render([], true, kind)), undefined, kind);
	}
});
