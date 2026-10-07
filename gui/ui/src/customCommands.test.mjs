import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const source = await readFile(new URL('./components/CustomCommands.tsx', import.meta.url), 'utf8');
const actions = [];
const compiled = ts.transpileModule(source, {
	compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const module = { exports: {} };

/** Exercise actual reducer-owned JSX with a deterministic command bridge. */
const mockRequire = (name) => {
	if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
	if (name === './popups') return { LAYER: { customCommand: 15.75 } };
	if (name === './Popup') return {
		Popup: ({ children, footer, onDismiss, dismissDisabled }) => React.createElement('section', null,
			React.createElement('button', { disabled: dismissDisabled, onClick: onDismiss }, 'Esc'), children, footer),
		PopupButton: (props) => React.createElement('button', props),
	};
	return require(name);
};
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Expand stateless JSX to the interactive host controls a browser receives. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Read only text children, keeping shell output separate from browser markup. */
function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

test('configured buttons preserve stable ids, optional hotkeys, and validated colors', () => {
	const buttons = [
		{ id: 7, name: 'Convert', hotkey: 'Ctrl+Alt+F', font_color: '#ffffff', background_color: '#224455' },
		{ id: 12, name: 'Open URL', hotkey: null, font_color: null, background_color: null },
	];
	const tree = module.exports.CustomCommandButtons({ buttons });
	const controls = nodes(tree).filter((node) => node.type === 'button');
	assert.equal(controls.length, 2);
	assert.equal(text(controls[0]), '[Ctrl+Alt+F] Convert');
	assert.equal(text(controls[1]), 'Open URL');
	assert.deepEqual(controls[0].props.style, { color: '#ffffff', backgroundColor: '#224455' });
	controls[0].props.onClick();
	controls[1].props.onClick();
	assert.deepEqual(actions.splice(0), [{ RunCustomCommand: 7 }, { RunCustomCommand: 12 }]);
	assert.equal(nodes(module.exports.CustomCommandButtons({ buttons: [] })).filter((node) => node.type === 'button').length, 0);
});

test('running command output keeps both close controls disabled', () => {
	const popup = { name: 'Convert', running: true, output: '', failed: false };
	const tree = module.exports.CustomCommandOutputPopup({ popup });
	const controls = nodes(tree).filter((node) => node.type === 'button');
	assert.ok(controls.length >= 1);
	assert.ok(controls.every((node) => node.props.disabled));
	for (const control of controls) control.props.onClick();
	assert.deepEqual(actions.splice(0), []);
	assert.ok(nodes(tree).some((node) => node.props.role === 'status'));
	assert.ok(text(tree).includes('Running...'));
	assert.doesNotMatch(text(tree), /[^\x00-\x7f]/);
});

test('completed output renders literal text and dismisses only through its semantic action', () => {
	for (const failed of [false, true]) {
		const popup = { name: 'Fixture', running: false, output: '<script>private output</script>\nsecond line', failed };
		const tree = module.exports.CustomCommandOutputPopup({ popup });
		const all = nodes(tree);
		assert.ok(all.every((node) => !node.props.dangerouslySetInnerHTML));
		assert.ok(text(tree).includes(popup.output));
		assert.match(text(tree), failed ? /failed/i : /finished/i);
		const close = all.find((node) => node.type === 'button' && text(node) === 'Close');
		assert.equal(Boolean(close.props.disabled), false);
		close.props.onClick();
		assert.deepEqual(actions.splice(0), ['DismissCustomCommandOutput']);
	}
});
