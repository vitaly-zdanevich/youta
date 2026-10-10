/** Site files are lazy core-owned text/list views, never HTML or external-browser links. */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const actions = [];
const scrolling = [];
const module = { exports: {} };
const source = readFileSync(new URL('./components/SiteFilePopup.tsx', import.meta.url), 'utf8');
const compiled = ts.transpileModule(source, {
	compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;

/** Only hooks, the virtual viewport and native IPC are replaced; controls are real JSX. */
const mockRequire = (name) => {
	if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }) };
	if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
	if (name === './popups') return { LAYER: { siteFile: 15.9 } };
	if (name === '../popupGeometry') return { reportEntryGeometry: () => {}, reportGeometry: () => {} };
	if (name === '@tanstack/react-virtual') return { useVirtualizer: ({ count }) => ({
		getVirtualItems: () => Array.from({ length: count }, (_, index) => ({ index, key: index, start: index * 46, size: 46 })),
		getTotalSize: () => count * 46, scrollToIndex: () => {}, measureElement: () => {},
	}) };
	if (name === './ScrollingText') return { ScrollingText: (props) => {
		scrolling.push(props);
		return React.createElement('pre', { 'data-site-file-text': true }, props.children);
	} };
	if (name === './Popup') return {
		Popup: ({ title, children, footer, onDismiss }) => React.createElement('section', null,
			React.createElement('h1', null, title), React.createElement('button', { onClick: onDismiss }, 'Esc'), children, footer),
		PopupError: ({ message }) => message ? React.createElement('p', { role: 'alert' }, message) : null,
	};
	return require(name);
};
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Expands function components while retaining their actual event handlers. */
function nodes(element) {
	if (Array.isArray(element)) return element.flatMap(nodes);
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Reads literal children without interpreting site-supplied markup. */
function text(element) {
	if (Array.isArray(element)) return element.map(text).join('');
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

const defaults = { title: 'robots.txt', url: 'https://example.com/robots.txt', loading: false, text: '',
	error: null, sitemap: false, sitemap_index: false, entries: [], selected: 0, can_go_back: false, scroll_offset: 0 };
const render = (values = {}) => module.exports.SiteFilePopup({ popup: { ...defaults, ...values } });

test('robots content stays complete literal text with core-owned scrolling and no fetching', () => {
	actions.length = 0;
	scrolling.length = 0;
	const body = `User-agent: *\nDisallow: /private\n<script>literal</script>\n${'line\n'.repeat(100)}`;
	const tree = render({ text: body, scroll_offset: 7 });
	const all = nodes(tree);
	assert.ok(all.every((node) => !node.props.dangerouslySetInnerHTML));
	assert.ok(all.every((node) => !['script', 'iframe', 'a', 'img'].includes(node.type)));
	assert.equal(text(all.find((node) => node.props['data-site-file-text'])), body);
	assert.deepEqual(actions, []);
	assert.equal(scrolling[0].popup, 'site_file');
	assert.equal(scrolling[0].offset, 7);
	scrolling[0].onScroll(9);
	assert.deepEqual(actions.splice(0), [{ SetSiteFileScroll: 9 }]);
});

test('loading and failure remain closable and do not show stale entries', () => {
	for (const state of [{ loading: true }, { error: 'Could not load sitemap' }]) {
		actions.length = 0;
		const tree = render({ ...state, sitemap: true, entries: state.loading ? [{ url: 'https://example.com/stale', metadata: [] }] : [] });
		const all = nodes(tree);
		assert.ok(!text(tree).includes('/stale'));
		assert.ok(all.some((node) => node.props.role === (state.loading ? 'status' : 'alert')));
		all.find((node) => node.type === 'button' && text(node) === 'Esc').props.onClick();
		assert.deepEqual(actions, ['DismissSiteFile']);
	}
});

test('an activation error keeps loaded sitemap rows visible for core keyboard navigation', () => {
	const tree = render({ sitemap: true, error: 'Web browsing is not enabled in this build',
		entries: [{ url: 'https://example.com/page', metadata: [['Last modified', '2025-01-01']] }] });
	const all = nodes(tree);
	assert.ok(all.some((node) => node.props.role === 'alert'));
	assert.ok(all.some((node) => node.props['data-site-file-entry'] === 0));
	assert.ok(text(tree).includes('Last modified: 2025-01-01'));
});

test('sitemap rows include each metadata field and activate only a core row index', () => {
	for (const sitemap_index of [false, true]) {
		actions.length = 0;
		const entries = [{ url: 'https://example.com/a.xml', metadata: [['Last modified', '2025-01-01'], ['Priority', '0.8']] },
			{ url: 'https://example.com/b', metadata: [['Title', '<img>literal</img>']] }];
		const tree = render({ title: 'sitemap.xml', sitemap: true, sitemap_index, entries, selected: 1 });
		const all = nodes(tree);
		const rows = all.filter((node) => node.props['data-site-file-entry'] !== undefined);
		assert.equal(rows.length, 2);
		assert.equal(rows[1].props['aria-current'], true);
		assert.ok(text(tree).includes('Last modified: 2025-01-01'));
		assert.ok(text(tree).includes('Priority: 0.8'));
		assert.ok(text(tree).includes('Title: <img>literal</img>'));
		assert.ok(all.every((node) => node.type !== 'a' && !node.props.dangerouslySetInnerHTML));
		assert.deepEqual(actions, []);
		rows[0].props.onClick({ stopPropagation() {} });
		assert.deepEqual(actions.splice(0), [{ ActivateSiteFileEntry: 0 }]);
		for (const key of ['Enter', ' ']) {
			const event = { key, preventDefault() {}, stopPropagation() {} };
			rows[1].props.onKeyDown(event);
			rows[1].props.onKeyDown({ ...event, repeat: true });
			assert.deepEqual(actions.splice(0), [{ ActivateSiteFileEntry: 1 }]);
		}
	}
});

test('parent Back is explicit, and Escape cancels even while a child is loading', () => {
	actions.length = 0;
	const tree = render({ sitemap: true, can_go_back: true, loading: true });
	const controls = nodes(tree).filter((node) => node.type === 'button');
	const back = controls.find((node) => text(node).includes('Back'));
	assert.ok(back);
	back.props.onClick({ stopPropagation() {} });
	assert.deepEqual(actions.splice(0), ['BackSiteFile']);
	back.props.onKeyDown({ key: 'Enter', preventDefault() {}, stopPropagation() {} });
	assert.deepEqual(actions.splice(0), ['BackSiteFile']);
	controls.find((node) => text(node) === 'Esc').props.onClick();
	assert.deepEqual(actions.splice(0), ['DismissSiteFile']);
	assert.ok(!nodes(render()).some((node) => node.type === 'button' && text(node).includes('Back')));
});
