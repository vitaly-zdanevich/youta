import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const source = await readFile(new URL('./components/popups.tsx', import.meta.url), 'utf8');
const contract = await readFile(new URL('./contract.ts', import.meta.url), 'utf8');
const app = await readFile(new URL('./App.tsx', import.meta.url), 'utf8');
const actions = [];

/** Execute the real JSX with a deterministic bridge and a portal-free popup shell. */
const compiled = ts.transpileModule(source, {
	compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const module = { exports: {} };
const mockRequire = (name) => {
	if (name === '../ipc') return { dispatch: (...args) => actions.push(args) };
	if (name === './Popup') return {
		Popup: ({ children, footer, onDismiss }) => React.createElement('section', null,
			React.createElement('button', { onClick: onDismiss }, 'Dismiss'), footer, children),
		PopupButton: (props) => React.createElement('button', props),
		PopupError: () => null,
	};
	if (name === './ScrollingText') return { ScrollingText: () => null };
	if (name === '../format') return { humanBytes: (value) => String(value) };
	return require(name);
};
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Expand function components into host elements without mounting a browser. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

test('download chooser renders reducer labels and confirms exact option indices', () => {
	assert.equal(typeof module.exports.DownloadChoicePopup, 'function');
	actions.length = 0;
	const popup = {
		generation: 7, title: 'Fixture <audio>', explanation: 'Choose an existing file.',
		options: ['Original <FLAC>', 'Archive MP3'], selected: 1,
	};
	const tree = module.exports.DownloadChoicePopup({ popup });
	const buttons = nodes(tree).filter((node) => node.type === 'button');
	for (const [index, label] of popup.options.entries()) {
		const button = buttons.find((node) => text(node) === label);
		assert.ok(button, label);
		assert.equal(button.props.emphasis, index === popup.selected);
		button.props.onClick();
		assert.deepEqual(actions.pop(), [{ SelectDownloadChoice: { generation: popup.generation, index } }]);
	}
	buttons.find((node) => text(node) === 'Download').props.onClick();
	assert.deepEqual(actions.pop(), [{ ConfirmDownloadChoice: popup.generation }]);
	buttons.find((node) => text(node) === 'Cancel').props.onClick();
	assert.deepEqual(actions.pop(), ['DismissDownloadChoice']);
	assert.ok(nodes(tree).every((node) => !node.props.dangerouslySetInnerHTML));
});

test('download preferences show typed values and gate unsupported source capabilities', () => {
	for (const [enabled, archiveSupported] of [[false, false], [false, true], [true, false], [true, true]]) {
		actions.length = 0;
		const popup = {
			auto_download_supported: enabled, download_mode: 'ask-each-time',
			archive_download_preference: 'original-file', subscriptions_layout: 'split',
			video_summary_supported: false, sponsorblock_supported: false,
			nyan_cat_supported: false,
		};
		const tree = module.exports.PreferencesPopup({ popup, archiveSupported });
		const buttons = nodes(tree).filter((node) => node.type === 'button');
		const mode = buttons.find((node) => text(node) === 'Ask each time');
		const archive = buttons.find((node) => text(node) === 'Original file');
		assert.equal(Boolean(mode), enabled);
		assert.equal(Boolean(archive), enabled && archiveSupported);
		if (enabled) {
			mode.props.onClick();
			assert.deepEqual(actions.pop(), ['CycleDownloadModePreference']);
		}
		if (enabled && archiveSupported) {
			archive.props.onClick();
			assert.deepEqual(actions.pop(), ['CycleArchiveDownloadPreference']);
		}
	}
});

test('window mounts the shared download chooser and declares the exact snapshot contract', () => {
	assert.match(contract, /export interface DownloadChoicePopupView/);
	assert.match(contract, /download_choice_popup: DownloadChoicePopupView \| null/);
	assert.match(contract, /download_mode: DownloadMode/);
	assert.match(contract, /archive_download_preference: ArchiveDownloadPreference/);
	assert.match(app, /view\.download_choice_popup/);
	assert.match(app, /<DownloadChoicePopup popup=\{view\.download_choice_popup\}/);
	assert.match(app, /archiveSupported=\{sources\.some\(\(source\) => source\.id === 'ArchiveOrg'\)\}/);
});
