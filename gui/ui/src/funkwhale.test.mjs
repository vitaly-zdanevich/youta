/** Funkwhale setup and pod selection against local fixtures and a mocked native bridge. */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const actions = [];
let slots = [];
let cursor = 0;
let effects = [];

/** Preserve hook state across explicit controller snapshots without mounting a DOM. */
const hooks = {
	...React,
	useState(initial) {
		const index = cursor++;
		if (!(index in slots)) slots[index] = initial;
		return [slots[index], (value) => { slots[index] = value; }];
	},
	useRef(initial) {
		const index = cursor++;
		if (!(index in slots)) slots[index] = { current: initial };
		return slots[index];
	},
	useCallback(callback, dependencies) {
		const index = cursor++;
		if (!(index in slots) || dependencies.some((value, part) => !Object.is(value, slots[index].dependencies[part]))) {
			slots[index] = { callback, dependencies };
		}
		return slots[index].callback;
	},
	useEffect(callback, dependencies) {
		const index = cursor++;
		if (!(index in slots) || dependencies.some((value, part) => !Object.is(value, slots[index][part]))) {
			slots[index] = dependencies;
			effects.push(callback);
		}
	},
};

/** Run the actual JSX while replacing only hooks, popup chrome, and native IPC. */
async function load(relative) {
	const source = await readFile(new URL(relative, import.meta.url), 'utf8').catch(() => '');
	const compiled = ts.transpileModule(source, {
		compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const module = { exports: {} };
	const mockRequire = (name) => {
		if (name === 'react') return hooks;
		if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
		if (name === '../searchPresentation') return searchPresentation;
		if (name === './popups') return { LAYER: { credentialEditor: 2 } };
		if (name === '@radix-ui/react-dialog') return Object.fromEntries(['Root', 'Portal', 'Content', 'Title']
			.map((name) => [name, ({ children, ...props }) => React.createElement('div', props, children)]));
		if (name === './Popup') return {
			isTopmostPopup: (target) => chrome.isTopmostPopup(target),
			Popup: ({ children, footer }) => React.createElement('section', null, children, footer),
			PopupButton: (props) => React.createElement('button', props),
			PopupError: ({ message }) => message ? React.createElement('p', { role: 'alert' }, message) : null,
		};
		return require(name);
	};
	new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);
	return module.exports;
}

const searchPresentation = await load('./searchPresentation.ts');
const component = await load('./components/FunkwhaleSetupPopup.tsx');
const search = await load('./components/SearchBar.tsx');
const tabs = await load('./components/Tabs.tsx');
const chrome = await load('./components/Popup.tsx');

/** Expand components but keep host elements and their event handlers inspectable. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Provider and error text stays inert React text, including markup-shaped fixtures. */
function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

/** Begin an independent dialog fixture and clear the recorded bridge actions. */
function reset() {
	slots = [];
	actions.length = 0;
}

/** Supply the exact public setup contract, never a browser-fetched pod list. */
function render(patch = {}, beforeEffects = () => {}) {
	assert.equal(typeof component.FunkwhaleSetupPopup, 'function');
	cursor = 0;
	effects = [];
	const tree = component.FunkwhaleSetupPopup({ popup: {
		url: 'https://music.example/', config_file_path: 'config.toml', error: null, picker: null, ...patch,
	} });
	beforeEffects(tree);
	for (const effect of effects) effect();
	return tree;
}

const button = (tree, label) => nodes(tree).find((node) => node.type === 'button'
	&& (text(node) === label || node.props['aria-label'] === label));

/** Radix may mount portal children after parent effects; focus belongs to the attached input. */
test('pod URL focuses on actual portal attachment and picker close without refocusing retained drafts', () => {
	reset();
	const input = nodes(render()).find((node) => node.type === 'input');
	assert.equal(typeof input.props.ref, 'function');
	let focused = 0;
	const node = { focus: () => { focused++; } };
	input.props.ref(node);
	assert.equal(focused, 1, 'the input receives focus even when portal attachment follows the initial effects');
	const retained = nodes(render({ error: 'Public fixture failure', config_file_path: 'redacted snapshot.toml' }))
		.find((node) => node.type === 'input');
	assert.equal(retained.props.ref, input.props.ref, 'a native snapshot must not detach/re-attach the ref or reset caret/focus');
	assert.equal(focused, 1);
	const picker = { pods: [], selected: 0, loading: true, error: null };
	const picking = nodes(render({ picker })).find((node) => node.type === 'input');
	picking.props.ref(node);
	assert.equal(focused, 1, 'the disabled URL does not steal pod-list focus');
	const returned = nodes(render()).find((node) => node.type === 'input');
	returned.props.ref(node);
	assert.equal(focused, 2, 'closing the list returns focus to its URL editor');
});

/** Ref attachment emits focus before passive effects copy an explicit pod-list selection. */
test('picker selection replaces the URL even when returning input focus precedes draft effects', () => {
	reset();
	render({ url: 'https://manual.example/' });
	render({ url: 'https://manual.example/', picker: { pods: [], selected: 0, loading: true, error: null } });
	const selectedUrl = 'https://selected.example/';
	render({ url: selectedUrl }, (tree) => {
		const input = nodes(tree).find((node) => node.type === 'input');
		input.props.ref({ focus: () => input.props.onFocus() });
	});
	assert.equal(nodes(render({ url: selectedUrl })).find((node) => node.type === 'input').props.value, selectedUrl);
});
const input = (tree) => nodes(tree).find((node) => node.type === 'input');
const picker = { pods: [], selected: 0, loading: false, error: null };

/** A DOM-shaped setup dialog lets the real popup layer guard run without a browser. */
function setupDialog(covered = false) {
	const dialog = { parentElement: { style: { zIndex: '250' } } };
	const other = { parentElement: { style: { zIndex: covered ? '1650' : '150' } } };
	return { closest: () => dialog, ownerDocument: { querySelectorAll: () => [other, dialog] } };
}

/** Observe both default prevention and propagation when a control owns a key. */
function key(props, name, patch = {}) {
	let prevented = false;
	let stopped = false;
	props.onKeyDown({ key: name, ctrlKey: false, altKey: false, metaKey: false, repeat: false, currentTarget: setupDialog(),
		preventDefault: () => { prevented = true; }, stopPropagation: () => { stopped = true; }, ...patch });
	return { prevented, stopped };
}

test('setup dialog accepts a manual URL and retains partial typing across redacted snapshots', () => {
	reset();
	const tree = render();
	assert.deepEqual(actions, [], 'opening setup must not persist or fetch pods');
	assert.equal(input(tree).props.value, 'https://music.example/');
	input(tree).props.onFocus();
	input(tree).props.onChange({ currentTarget: { value: 'https://' } });
	assert.deepEqual(actions, [{ SetFunkwhalePodUrl: 'https://' }]);
	assert.equal(input(render({ url: '' })).props.value, 'https://', 'safe empty projections must not erase the local draft');
	input(render({ url: '' })).props.onChange({ currentTarget: { value: 'https://custom.example/' } });
	assert.deepEqual(actions.at(-1), { SetFunkwhalePodUrl: 'https://custom.example/' });
});

/** Esc from discovery is not a selection: an empty safe projection cannot erase a private partial URL. */
test('cancelling pod discovery preserves the partial manual URL while restoring input focus', () => {
	reset();
	const initial = input(render());
	initial.props.onFocus();
	initial.props.onChange({ currentTarget: { value: 'https://partial-' } });
	render({ url: '', picker: { pods: [], selected: 0, loading: true, error: null } });
	render({ url: '' }, (tree) => {
		const returned = input(tree);
		returned.props.ref({ focus: () => returned.props.onFocus() });
	});
	assert.equal(input(render({ url: '' })).props.value, 'https://partial-');
});

test('manual setup controls save, clear, and cancel through explicit core actions', () => {
	reset();
	const tree = render({ error: 'Check the pod URL.' });
	assert.ok(text(tree).includes('config.toml'));
	assert.ok(nodes(tree).some((node) => node.props.role === 'alert' && text(node) === 'Check the pod URL.'));
	button(tree, 'Save').props.onClick();
	assert.equal(actions.pop(), 'SaveFunkwhaleSetup');
	assert.deepEqual(key(input(tree).props, 'Enter'), { prevented: true, stopped: true });
	assert.equal(actions.pop(), 'SaveFunkwhaleSetup');
	assert.deepEqual(key(input(tree).props, 'u', { ctrlKey: true }), { prevented: true, stopped: true });
	assert.equal(actions.pop(), 'ClearFunkwhalePodUrl');
	assert.equal(input(render({ url: '' })).props.value, '');
	assert.deepEqual(key(input(tree).props, 'Escape'), { prevented: true, stopped: true });
	assert.equal(actions.pop(), 'CloseFunkwhaleSetup');
	button(tree, 'Cancel').props.onClick();
	assert.equal(actions.pop(), 'CloseFunkwhaleSetup');
	button(tree, 'Choose public pod').props.onClick();
	assert.equal(actions.pop(), 'OpenFunkwhalePodPicker');
});

test('public pod rows display server labels, URLs, and registration status as text', () => {
	reset();
	const pods = [true, false, null].map((open_registrations, index) => ({
		label: index === 0 ? '<script>fixture</script>' : `Fixture ${index}`,
		url: `https://pod-${index}.example/`, open_registrations,
	}));
	const tree = render({ picker: { ...picker, pods, selected: 1 } });
	const rows = nodes(tree).filter((node) => node.props.role === 'option');
	assert.equal(rows.length, 3);
	assert.equal(rows[1].props['aria-selected'], true);
	assert.ok(text(tree).includes('https://pod-0.example/'));
	assert.ok(text(tree).includes('Registration open'));
	assert.ok(text(tree).includes('Registration closed'));
	assert.ok(text(tree).includes('<script>fixture</script>'));
	assert.ok(nodes(tree).every((node) => !node.props.dangerouslySetInnerHTML));
	assert.equal(input(tree).props.disabled, true);
	assert.equal(button(tree, 'Save').props.disabled, true);
	rows[2].props.onClick();
	assert.deepEqual(actions.pop(), { SelectFunkwhalePod: 2 }, 'selection fills the draft without saving');
	assert.deepEqual(key(rows[1].props, 'ArrowDown'), { prevented: true, stopped: true });
	assert.deepEqual(actions.pop(), { MoveFunkwhalePod: 1 });
	key(rows[1].props, 'ArrowUp');
	assert.deepEqual(actions.pop(), { MoveFunkwhalePod: -1 });
	key(rows[1].props, 'Enter');
	assert.deepEqual(actions.pop(), { SelectFunkwhalePod: 1 });
	key(rows[1].props, 'Escape');
	assert.equal(actions.pop(), 'CloseFunkwhalePodPicker');
	tree.props.onDismiss();
	assert.equal(actions.pop(), 'CloseFunkwhalePodPicker');
});

/** Clearing the optional provider address uses the same explicit persistence action. */
test('an empty pod draft remains saveable to remove the configured pod', () => {
	reset();
	const tree = render({ url: '' });
	assert.equal(button(tree, 'Save').props.disabled, false);
	button(tree, 'Save').props.onClick();
	assert.equal(actions.pop(), 'SaveFunkwhaleSetup');
	assert.ok(text(tree).includes('remove the saved pod'));
});

/** Header activation must cancel, rather than reaching the modal Enter/save shortcut. */
test('popup header Enter and Space dismiss once without reaching the document keymap', () => {
	let dismissed = 0;
	const dialog = { parentElement: { style: { zIndex: '250' } } };
	const lower = { parentElement: { style: { zIndex: '150' } } };
	const currentTarget = { closest: () => dialog, ownerDocument: { querySelectorAll: () => [dialog, lower] } };
	const tree = chrome.Popup({ title: 'Funkwhale pod setup', layer: 2, dismissLabel: 'Cancel',
		onDismiss: () => { dismissed++; }, children: null });
	const close = button(tree, 'Cancel');
	assert.equal(typeof close.props.onKeyDown, 'function');
	for (const name of ['Enter', ' ']) {
		assert.deepEqual(key(close.props, name, { currentTarget }), { prevented: true, stopped: true });
	}
	assert.equal(dismissed, 2);
	key(close.props, 'Enter', { currentTarget, repeat: true });
	assert.equal(dismissed, 2, 'held activation must not dismiss a second modal');
});

/** A covered header must leave activation to the shared keymap, regardless of portal order. */
test('covered popup header Enter and Space do not dismiss or stop the shared keyboard route', () => {
	let dismissed = 0;
	const dialog = { parentElement: { style: { zIndex: '250' } } };
	const higher = { parentElement: { style: { zIndex: '1650' } } };
	const currentTarget = { closest: () => dialog, ownerDocument: { querySelectorAll: () => [higher, dialog] } };
	const tree = chrome.Popup({ title: 'Funkwhale pod setup', layer: 2, dismissLabel: 'Cancel',
		onDismiss: () => { dismissed++; }, children: null });
	const close = button(tree, 'Cancel');
	for (const name of ['Enter', ' ']) {
		assert.deepEqual(key(close.props, name, { currentTarget }), { prevented: false, stopped: false });
	}
	assert.equal(dismissed, 0, 'the higher popup owns keyboard activation');
});

/** Native URL editing requires keyboard access to each explicit dialog control. */
test('setup owns native Tab focus and wraps within the dialog instead of source tabs', () => {
	reset();
	const tree = render();
	assert.equal(typeof tree.props.onKeyDown, 'function');
	let focused = null;
	const controls = [0, 1, 2].map((index) => ({ tabIndex: 0, focus: () => { focused = index; } }));
	const currentTarget = { ...setupDialog(), querySelectorAll: () => controls };
	assert.deepEqual(key(tree.props, 'Tab', { target: controls[1], currentTarget }), { prevented: false, stopped: true });
	assert.equal(focused, null, 'the browser advances between ordinary controls');
	assert.deepEqual(key(tree.props, 'Tab', { target: controls[2], currentTarget }), { prevented: true, stopped: true });
	assert.equal(focused, 0);
	assert.deepEqual(key(tree.props, 'Tab', { target: controls[0], currentTarget, shiftKey: true }), { prevented: true, stopped: true });
	assert.equal(focused, 2);
	assert.deepEqual(actions, [], 'Tab focus must neither switch source nor save/select');
});

/** Button focus must not let the core edit a draft the local URL input still displays differently. */
test('typing and clearing while a setup button is focused also update the visible URL draft', () => {
	reset();
	let tree = render({ url: 'https://music.example/👩‍🔬' });
	const target = {};
	assert.deepEqual(key(tree.props, 'Backspace', { target }), { prevented: true, stopped: true });
	assert.deepEqual(actions.pop(), { SetFunkwhalePodUrl: 'https://music.example/' });
	tree = render({ url: 'https://music.example/' });
	assert.equal(input(tree).props.value, 'https://music.example/');
	key(tree.props, 'x', { target });
	assert.deepEqual(actions.pop(), { SetFunkwhalePodUrl: 'https://music.example/x' });
	tree = render({ url: 'https://music.example/x' });
	assert.equal(input(tree).props.value, 'https://music.example/x');
	assert.deepEqual(key(tree.props, 'u', { target, ctrlKey: true }), { prevented: true, stopped: true });
	assert.equal(actions.pop(), 'ClearFunkwhalePodUrl');
	assert.equal(input(render({ url: '' })).props.value, '');
});

/** Bubbling from a covered header must not edit the parked setup draft or consume the upper popup's keys. */
test('covered setup dialog preserves its draft and forwards header Enter and Space', () => {
	reset();
	const url = 'https://';
	const tree = render({ url });
	for (const name of ['Enter', ' ']) {
		const result = key(tree.props, name, { target: {}, currentTarget: setupDialog(true) });
		assert.deepEqual(actions, [], 'covered setup must not dispatch URL edits or control actions');
		assert.equal(input(render({ url })).props.value, url, 'covered setup must preserve the local draft');
		assert.deepEqual(result, { prevented: false, stopped: false });
	}
});

test('loading, empty and failed pod lists remain closable and retryable', () => {
	for (const patch of [{ loading: true }, {}, { error: '<img src=x onerror=alert(1)>' }]) {
		reset();
		const tree = render({ picker: { ...picker, ...patch } });
		assert.equal(nodes(tree).filter((node) => node.props.role === 'option').length, 0);
		assert.ok(text(tree).includes(patch.loading ? 'Loading public pods' : patch.error ?? 'No public pods'));
		button(tree, 'Retry').props.onClick();
		assert.equal(actions.pop(), 'OpenFunkwhalePodPicker');
		button(tree, 'Close pod list').props.onClick();
		assert.equal(actions.pop(), 'CloseFunkwhalePodPicker');
	}
});

test('the search header offers pod setup only on Funkwhale and tabs preserve native catalogue order', () => {
	for (const screen of ['Funkwhale', 'Bandcamp', 'Search']) {
		const tree = search.SearchBar({ view: { screen, search_query: '', search_cursor_byte: 0, subscriptions: {} },
			verb: 'Search', label: screen });
		assert.equal(Boolean(button(tree, 'Pod setup')), screen === 'Funkwhale');
		if (screen === 'Funkwhale') {
			button(tree, 'Pod setup').props.onClick();
			assert.equal(actions.pop(), 'OpenFunkwhaleSetup');
		}
	}
	const sources = ['Bandcamp', 'Funkwhale', 'ApplePodcasts'].map((id) => ({ id, label: id }));
	const tree = tabs.Tabs({ sources, active: 'Funkwhale', playing: null, paused: false });
	assert.deepEqual(nodes(tree).filter((node) => node.type === 'button').map(text), sources.map((source) => source.id));
	button(tree, 'Funkwhale').props.onClick();
	assert.deepEqual(actions.pop(), { ShowScreen: 'Funkwhale' });
});

test('GUI setup declarations mirror the shared contract and default-removable feature', async () => {
	const [contract, app, cargo] = await Promise.all([
		readFile(new URL('./contract.ts', import.meta.url), 'utf8'),
		readFile(new URL('./App.tsx', import.meta.url), 'utf8'),
		readFile(new URL('../../Cargo.toml', import.meta.url), 'utf8'),
	]);
	const parsed = ts.createSourceFile('contract.ts', contract, ts.ScriptTarget.Latest, true);
	for (const [name, fields] of [
		['FunkwhaleSetupPopupView', ['url', 'config_file_path', 'error', 'picker']],
		['FunkwhalePodPickerView', ['pods', 'selected', 'loading', 'error']],
		['FunkwhalePodView', ['label', 'url', 'open_registrations']],
	]) {
		const type = parsed.statements.find((node) => ts.isInterfaceDeclaration(node) && node.name.text === name);
		assert.ok(type, `${name} must mirror the Rust projection`);
		assert.deepEqual(type.members.map((node) => node.name.text), fields);
	}
	assert.match(app, /<FunkwhaleSetupPopup popup=\{view\.funkwhale_setup_popup\}/);
	assert.match(cargo, /^default = \[[^\n]*"funkwhale"/m);
	assert.match(cargo, /^funkwhale = \["youta\/funkwhale"\]$/m);
});
