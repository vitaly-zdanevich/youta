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
/** Exercise actual JSX against a deterministic bridge, without portals or browser state. */
const mockRequire = (name) => {
	if (name === '../ipc') return { dispatch: (...args) => actions.push(args) };
	if (name === './Popup') return {
		Popup: ({ children, footer }) => React.createElement('section', null, footer, children),
		PopupButton: (props) => React.createElement('button', props),
		PopupError: () => null,
	};
	if (name === './ScrollingText') return { ScrollingText: () => null };
	if (name === '../format') return { humanBytes: (value) => String(value) };
	return require(name);
};
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Expand stateless reducer-owned components into their interactive host elements. */
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

function fixture() {
	return {
		generation: 42, selected_field: 'Description', phase: 'Review', animation_frame: 0,
		uploaded_bytes: 0, total_bytes: null, validation_error: null, result_url: null,
		draft: {
			identifier: 'fixture-item', title: 'Fixture title', description: 'First line\nSecond line',
			creator: 'Fixture creator', source_url: 'https://www.youtube.com/watch?v=abcdefghijk',
			upload_video: false,
		},
	};
}

test('Archive upload has one video checkbox and requires an explicit generation-bound submission', () => {
	assert.equal(typeof module.exports.ArchiveUploadPopup, 'function');
	for (const generation of [7, 42]) {
		const popup = { ...fixture(), generation };
		const tree = module.exports.ArchiveUploadPopup({ popup });
		const buttons = nodes(tree).filter((node) => node.type === 'button');
		const upload = buttons.find((node) => text(node) === 'Upload');
		assert.ok(upload);
		assert.equal(Boolean(upload.props.disabled), false);
		upload.props.onClick();
		assert.deepEqual(actions.pop(), [{ SubmitArchiveUpload: generation }]);
		assert.ok(text(tree).includes('First line\nSecond line'));
		assert.ok(!text(tree).includes('Source:'));
		assert.ok(!text(tree).includes(popup.draft.source_url));
		assert.ok(!text(tree).includes('permission'));
		assert.ok(!text(tree).includes('F3'));
		assert.ok(text(tree).includes('Opus'));
		const checkboxes = buttons.filter((node) => node.props.role === 'checkbox');
		assert.equal(checkboxes.length, 1);
		assert.ok(text(checkboxes[0]).includes('Upload video'));
		checkboxes[0].props.onClick();
		assert.deepEqual(actions.pop(), ['ToggleArchiveUploadVideo']);
		assert.ok(nodes(tree).every((node) => !node.props.dangerouslySetInnerHTML));
	}
});

test('Archive upload progress cannot submit again and can cancel or open the result', () => {
	assert.equal(typeof module.exports.ArchiveUploadPopup, 'function');
	for (const phase of ['Preparing', 'Uploading', 'Cancelling']) {
		const popup = { ...fixture(), phase, uploaded_bytes: 50, total_bytes: 100 };
		const tree = module.exports.ArchiveUploadPopup({ popup });
		const buttons = nodes(tree).filter((node) => node.type === 'button');
		assert.ok(!buttons.some((node) => text(node) === 'Upload' && !node.props.disabled));
		buttons.find((node) => text(node) === 'Cancel').props.onClick();
		assert.deepEqual(actions.pop(), ['DismissArchiveUpload']);
		if (phase === 'Uploading') assert.ok(text(tree).includes('50%'));
	}
	for (const phase of ['Failed', 'Cancelled']) {
		const tree = module.exports.ArchiveUploadPopup({ popup: { ...fixture(), phase } });
		const buttons = nodes(tree).filter((node) => node.type === 'button');
		assert.ok(!buttons.some((node) => text(node) === 'Upload'));
		assert.ok(buttons.some((node) => text(node) === 'Close'));
		assert.ok(buttons.filter((node) => node.props.role === 'checkbox').every((node) => node.props.disabled));
	}
	const tree = module.exports.ArchiveUploadPopup({ popup: {
		...fixture(), phase: 'Complete', result_url: 'https://archive.org/details/fixture-item',
	} });
	assert.ok(text(tree).includes('Upload accepted; archive.org may still be processing.'));
	nodes(tree).find((node) => node.type === 'button' && text(node) === 'Open item').props.onClick();
	assert.deepEqual(actions.pop(), ['OpenArchiveUploadResult']);
});


test('Archive Details control follows Commons and Evernote and requires selected YouTube capability', async () => {
	const details = await readFile(new URL('./components/Details.tsx', import.meta.url), 'utf8');
	const guard = details.match(/\{([^\n]+) \? \(\s*<Action onClick=\{\(\) => void dispatch\('OpenArchiveUpload'\)\}>Upload to archive\.org<\/Action>/)?.[1];
	assert.ok(guard, 'Archive action must keep its explicit capability/source guard');
	for (const archive_upload_supported of [false, true]) {
		for (const archive_upload_available of [false, true]) {
			for (const isYouTube of [false, true]) {
				assert.equal(new Function('view', 'isYouTube', `return (${guard});`)(
					{ archive_upload_supported, archive_upload_available }, isYouTube,
				), archive_upload_supported && archive_upload_available && isYouTube);
			}
		}
	}
	assert.ok(details.indexOf('>Upload to Commons</Action>') < details.indexOf('>Save audio to Evernote</Action>'));
	assert.ok(details.indexOf('>Save audio to Evernote</Action>') < details.indexOf('>Upload to archive.org</Action>'));
	assert.ok(!details.includes('[I] Upload to archive.org'));
});

test('Archive window mounts the public popup and a credentials projection containing no keys', async () => {
	const contract = await readFile(new URL('./contract.ts', import.meta.url), 'utf8');
	const app = await readFile(new URL('./App.tsx', import.meta.url), 'utf8');
	const fields = contract.match(/export interface ArchiveCredentialsEditorView \{([^}]+)\}/)?.[1];
	assert.ok(fields);
	assert.deepEqual(fields.trim().split('\n').map((line) => line.trim().split(':')[0]), [
		'access_key_length', 'secret_key_length', 'secret_selected', 'validation_failed',
	]);
	const draftFields = contract.match(/export interface ArchiveUploadDraft \{([^}]+)\}/)?.[1];
	assert.ok(draftFields.includes('source_url: string'));
	assert.ok(!draftFields.includes('rights_confirmed'));
	assert.match(app, /<ArchiveUploadPopup popup=\{view\.archive_upload_popup\}/);
	assert.match(app, /<ArchiveCredentialsPopup editor=\{view\.archive_credentials_editor\}/);
	assert.ok(source.includes("['I', 'upload selected YouTube media to archive.org']"));
});

test('Archive credentials render only masked editor state and explicitly remain session-only', () => {
	assert.equal(typeof module.exports.ArchiveCredentialsPopup, 'function');
	const editor = { access_key_length: 12, secret_key_length: 18, secret_selected: true, validation_failed: true };
	const tree = module.exports.ArchiveCredentialsPopup({ editor });
	assert.ok(text(tree).includes('not saved'));
	assert.ok(text(tree).includes('https://archive.org/account/s3.php'));
	assert.ok(text(tree).includes('Get archive.org upload keys'));
	assert.ok(text(tree).includes('secrets/archive-org.toml'));
	assert.ok(text(tree).includes('default ~/.config/youta/secrets/archive-org.toml'));
	assert.ok(text(tree).includes("access_key = 'YOUR_ACCESS_KEY'"));
	assert.ok(text(tree).includes("secret_key = 'YOUR_SECRET_KEY'"));
	assert.ok(text(tree).indexOf('Secret key') < text(tree).indexOf('secrets/archive-org.toml'));
	const buttons = nodes(tree).filter((node) => node.type === 'button');
	buttons.find((node) => text(node).includes('Secret key')).props.onClick();
	assert.deepEqual(actions.pop(), [{ SelectArchiveCredentialField: true }]);
	buttons.find((node) => text(node) === 'Use for session').props.onClick();
	assert.deepEqual(actions.pop(), ['SubmitArchiveCredentials']);
});
