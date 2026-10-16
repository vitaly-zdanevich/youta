/** Empty-search presentation uses real JSX and inert reducer snapshots, without network requests. */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const actions = [];
const base = { screen: 'Search', search_query: '', search_cursor_byte: 0, search_editing: false,
	rows: [], details: null, search_activity: null, help_open: false,
	subscriptions: { show_youtube_shorts: true }, playback: { idle: true }, playback_history_enabled: true };

/** Load the actual component while replacing only React effects and native boundaries. */
async function load(relative, extra = {}) {
	const source = await readFile(new URL(relative, import.meta.url), 'utf8');
	const module = { exports: {} };
	const compiled = ts.transpileModule(source, {
		compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const mockRequire = (name) => {
		if (name in extra) return extra[name];
		if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }), useCallback: (fn) => fn };
		if (name === '../ipc' || name === './ipc') return { dispatch: (action) => actions.push(action) };
		if (name === '../searchPresentation' || name === './searchPresentation') return presentation;
		if (name === './components/SearchBar') return search;
		if (name.startsWith('./')) return new Proxy({}, { get: (_, key) => key });
		return require(name);
	};
	new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);
	return module.exports;
}
const presentation = await load('./searchPresentation.ts');
const search = await load('./components/SearchBar.tsx');

/** Traverse host children while retaining inspectable event handlers. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}
/** Read text exactly as supplied by JSX, ignoring inert component boundaries. */
function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}
const renderSearch = (patch = {}) => search.SearchBar({ view: { ...base, ...patch }, verb: 'Search', label: 'Fixture' });

test('empty idle queries hide the editor while independent source controls stay accessible', () => {
	for (const screen of ['Search', 'Funkwhale', 'SoundCloud', 'ArchiveOrg', 'YouTubeMusic']) {
		const tree = renderSearch({ screen, archive_org_back_available: true, soundcloud_back_available: true });
		assert.equal(nodes(tree).some((node) => node.props.onMouseDown), false, screen);
		assert.doesNotMatch(text(tree), /Search Fixture|Press \/|⌕/);
		const button = nodes(tree).find((node) => node.type === 'button');
		if (screen === 'YouTubeMusic') assert.equal(tree, null);
		else assert.ok(button, `${screen} must retain its independent control`);
	}
	for (const patch of [{ search_editing: true }, { search_query: 'retained query' }]) {
		assert.ok(nodes(renderSearch(patch)).some((node) => node.props.onMouseDown));
	}
	assert.ok(nodes(renderSearch({ screen: 'Funkwhale', search_activity: 'Funkwhale' }))
		.some((node) => node.props['aria-label'] === 'Loading Fixture'));
});

test('empty-body prompt includes completed empty searches but not loading, selections, errors or browse routes', () => {
	for (const query of ['', 'missing fixture']) assert.equal(presentation.emptySearchPromptVisible({ ...base, search_query: query }), true);
	for (const patch of [
		{ search_editing: true }, { search_activity: 'YouTube' }, { rows: [{}] }, { details: {} },
		{ help_open: true }, { error_popup: {} }, { preferences_popup: {} },
		{ private_note_open: true }, { local_command_open: true }, { local_file_progress: {} },
		{ screen: 'Local' }, { screen: 'Web' }, { screen: 'Radio' },
		...['Artist', 'Album', 'Recommendations'].map((yandex_music_route) => ({ screen: 'YandexMusic', yandex_music_route })),
	]) assert.equal(presentation.emptySearchPromptVisible({ ...base, ...patch }), false, JSON.stringify(patch));
});

/** VK uses the same empty-search guidance, without covering setup instructions. */
test('VK hides an empty query and keeps setup or loading visible', () => {
	assert.equal(presentation.searchEditorVisible({ ...base, screen: 'Vk' }), false);
	assert.equal(presentation.emptySearchPromptVisible({ ...base, screen: 'Vk' }), true);
	assert.equal(presentation.emptySearchPromptVisible({ ...base, screen: 'Vk', details: { title: 'VK Music setup' } }), false);
	assert.equal(presentation.emptySearchPromptVisible({ ...base, screen: 'Vk', search_activity: 'Vk' }), false);
});

test('nonempty catalogues retain a compact mouse search button without a duplicate empty editor', () => {
	for (const patch of [{ rows: [{}] }, { details: {} }]) {
		actions.length = 0;
		const tree = renderSearch({ screen: 'YouTubeMusic', ...patch });
		const control = nodes(tree).find((node) => node.type === 'button' && text(node) === 'Search');
		assert.ok(control);
		assert.equal(nodes(tree).some((node) => node.props.onMouseDown), false);
		let blurred = false;
		control.props.onClick({ currentTarget: { blur: () => { blurred = true; } } });
		assert.equal(blurred, true, 'the native query editor must receive subsequent typing');
		assert.deepEqual(actions, ['BeginSearch']);
	}
});

/** Nested music catalogues expose the same reversible navigation as the TUI. */
test('Funkwhale and VK Back stay available with an empty search row', () => {
	for (const [screen, available] of ['Funkwhale', 'Vk'].flatMap((screen) => [false, true].map((available) => [screen, available]))) {
		actions.length = 0;
		const tree = renderSearch({ screen, funkwhale_back_available: available, vk_back_available: available });
		const back = nodes(tree).find((node) => node.type === 'button' && text(node).includes('Back'));
		assert.equal(Boolean(back), available);
		if (back) {
			back.props.onClick();
			assert.deepEqual(actions, ['GoBack']);
		}
	}
});

test('empty guidance spans the whole main area and clicking begins the existing reducer search', async () => {
	const app = await load('./App.tsx', { './useYouta': { useYouta: () => ({ view: base,
		sources: [{ id: 'Search', label: 'YT', search_verb: 'Search' }], output: null, failure: null }) } });
	actions.length = 0;
	const main = nodes(app.App()).find((node) => node.type === 'main');
	const prompt = nodes(main).find((node) => text(node) === 'To search, press /' && node.type === 'button');
	assert.ok(prompt);
	assert.match(prompt.props.className, /col-span-2/);
	assert.match(prompt.props.className, /items-center/);
	assert.match(prompt.props.className, /justify-center/);
	assert.equal(nodes(main).some((node) => node.type === 'RowList' || node.type === 'Details'), false);
	prompt.props.onClick({ currentTarget: { blur: () => {} } });
	assert.deepEqual(actions, ['BeginSearch']);
	for (const key of ['Enter', ' ']) {
		actions.length = 0;
		let prevented = false;
		let stopped = false;
		let blurred = false;
		prompt.props.onKeyDown({ key, repeat: false, preventDefault: () => { prevented = true; },
			stopPropagation: () => { stopped = true; }, currentTarget: { blur: () => { blurred = true; } } });
		assert.ok(prevented && stopped && blurred, 'activation cannot reach document playback shortcuts or retain query focus');
		assert.deepEqual(actions, ['BeginSearch']);
		prompt.props.onKeyDown({ key, repeat: true, preventDefault: () => {}, stopPropagation: () => {}, currentTarget: { blur: () => {} } });
		assert.deepEqual(actions, ['BeginSearch'], 'held activation never dispatches twice');
	}
});
