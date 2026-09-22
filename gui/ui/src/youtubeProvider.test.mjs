/** Provider editor controls against a deterministic bridge, without secrets or network access. */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const source = await readFile(new URL('./components/popups.tsx', import.meta.url), 'utf8');
const actions = [];
const compiled = ts.transpileModule(source, {
	compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const module = { exports: {} };
/** DOM scrolling is covered separately in Firefox; these fixtures exercise actual JSX/actions. */
const mockRequire = (name) => {
	if (name === 'react') return { ...React, useRef: () => ({ current: null }), useEffect: () => {} };
	if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
	if (name === './Popup') return {
		Popup: ({ children, footer }) => React.createElement('section', null, children, footer),
		PopupButton: (props) => React.createElement('button', props),
		PopupError: () => null,
	};
	if (name === './ScrollingText') return { ScrollingText: () => null };
	if (name === '../format') return { humanBytes: String };
	return require(name);
};
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Expand components while preserving interactive host nodes and their event handlers. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Render text exactly as React children, never interpreting provider strings as markup. */
function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

/** Only lengths and validated public URLs appear in a provider projection. */
function fixture(patch = {}) {
	return { selected_field: 'ApiKey', api_key_length: 23, invidious_url_length: 31,
		invidious_url: null, invidious_instances: null, validation_failed: false,
		from_preferences: true, official_supported: true, invidious_supported: true, ...patch };
}
const picker = { loading: false, loading_frame: 0, instances: [], selected: 0, error: null };
const render = (patch = {}) => module.exports.YouTubeProviderPopup({ editor: fixture(patch) });
const button = (tree, label) => nodes(tree).find((node) => node.type === 'button'
	&& (text(node) === label || node.props['aria-label'] === label));

test('provider controls remain masked and dispatch selection separately from save', () => {
	actions.length = 0;
	const tree = render();
	assert.deepEqual(actions, [], 'rendering must not fetch the directory or persist settings');
	assert.ok(text(tree).includes('23 characters entered'));
	assert.ok(text(tree).includes('31 characters entered'));
	assert.ok(!nodes(tree).some((node) => ['input', 'textarea'].includes(node.type)));
	for (const [label, field] of [['YouTube API key', 'ApiKey'], ['Invidious instance URL', 'InvidiousUrl']]) {
		button(tree, label).props.onClick();
		assert.deepEqual(actions.pop(), { SelectYouTubeSetupField: field });
	}
	button(tree, 'Choose instance').props.onClick();
	assert.equal(actions.pop(), 'OpenInvidiousInstancePicker');
	button(tree, 'Save').props.onClick();
	assert.equal(actions.pop(), 'SubmitYouTubeSetup');
	assert.ok(button(render({ from_preferences: false }), 'Save and retry'));
	button(tree, 'Cancel').props.onClick();
	assert.equal(actions.pop(), 'DismissYouTubeSetup');
});

test('directory rows fill the URL without saving and Escape closes only the directory', () => {
	const instances = [{ url: 'https://instance.example/', label: 'instance.example (Fixture)' }];
	const tree = render({ invidious_instances: { ...picker, instances } });
	const row = nodes(tree).find((node) => node.props.role === 'option');
	assert.equal(row.props['aria-selected'], true);
	actions.length = 0;
	row.props.onClick();
	assert.deepEqual(actions, [{ SelectInvidiousInstance: 0 }]);
	assert.equal(button(tree, 'Save').props.disabled, true);
	assert.equal(button(tree, 'YouTube API key').props.disabled, true);
	button(tree, 'Use selected instance').props.onClick();
	assert.equal(actions.pop(), 'ConfirmInvidiousInstance');
	tree.props.onDismiss();
	assert.equal(actions.pop(), 'DismissInvidiousInstancePicker');
	const closed = render({ invidious_url: instances[0].url });
	assert.ok(text(closed).includes(instances[0].url));
	closed.props.onDismiss();
	assert.equal(actions.pop(), 'DismissYouTubeSetup');
});

test('directory loading animation, error and empty results stay retryable but not selectable', () => {
	for (const [loading_frame, frame] of ['|', '/', '-', '\\', '|'].entries()) {
		const tree = render({ invidious_instances: { ...picker, loading: true, loading_frame } });
		assert.ok(text(nodes(tree).find((node) => node.props.role === 'status')).startsWith(frame));
		assert.equal(button(tree, 'Use selected instance').props.disabled, true);
		button(tree, 'Retry').props.onClick();
		assert.equal(actions.pop(), 'OpenInvidiousInstancePicker');
	}
	for (const error of [null, '<img src=x onerror=alert(1)>']) {
		const tree = render({ invidious_instances: { ...picker, error } });
		assert.equal(button(tree, 'Use selected instance').props.disabled, true);
		assert.ok(text(tree).includes(error ?? 'No public instances are available'));
		assert.ok(nodes(tree).every((node) => !node.props.dangerouslySetInnerHTML));
		assert.ok(button(tree, 'Close instance list'));
	}
});

test('provider feature gates omit unsupported fields and do not expose a disabled directory', () => {
	assert.ok(!button(render({ official_supported: false }), 'YouTube API key'));
	const officialOnly = render({ invidious_supported: false, invidious_instances: picker });
	assert.ok(!button(officialOnly, 'Invidious instance URL'));
	assert.ok(!button(officialOnly, 'Choose instance'));
	assert.ok(!button(officialOnly, 'Retry'));
	assert.equal(button(officialOnly, 'Save').props.disabled, false);
	assert.equal(button(render({ official_supported: false, invidious_supported: false }), 'Save').props.disabled, true);
	for (const youtube_provider_settings_supported of [false, true]) {
		const tree = module.exports.PreferencesPopup({ popup: { youtube_provider_settings_supported }, archiveSupported: false });
		assert.equal(Boolean(button(tree, 'YouTube API / Invidious…')), youtube_provider_settings_supported);
		if (youtube_provider_settings_supported) {
			button(tree, 'YouTube API / Invidious…').props.onClick();
			assert.equal(actions.pop(), 'OpenYouTubeProviderSettings');
		}
	}
});

test('the GUI mounts a redacted provider projection instead of its old unsupported notice', async () => {
	const contract = await readFile(new URL('./contract.ts', import.meta.url), 'utf8');
	const app = await readFile(new URL('./App.tsx', import.meta.url), 'utf8');
	const parsed = ts.createSourceFile('contract.ts', contract, ts.ScriptTarget.Latest, true);
	const editor = parsed.statements.find((node) => ts.isInterfaceDeclaration(node) && node.name.text === 'YouTubeProviderEditorView');
	assert.deepEqual(editor.members.map((node) => node.name.text), [
		'selected_field', 'api_key_length', 'invidious_url_length', 'invidious_url', 'invidious_instances',
		'validation_failed', 'from_preferences', 'official_supported', 'invidious_supported',
	]);
	assert.match(app, /<YouTubeProviderPopup editor=\{view\.youtube_provider_editor\}/);
	assert.ok(!app.includes('youtube_setup_open'));
	assert.ok(!source.includes('YouTube credentials needed'));
	assert.ok(nodes(render({ validation_failed: true })).some((node) => node.props.role === 'alert'));
});
