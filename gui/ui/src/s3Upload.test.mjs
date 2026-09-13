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
/** Exercise actual popup JSX without network, credentials, or an external window. */
const mockRequire = (name) => {
	if (name === '../ipc') return { dispatch: (...args) => actions.push(args) };
	if (name === './Popup') return {
		Popup: ({ children, footer }) => React.createElement('section', null, footer, children),
		PopupButton: (props) => React.createElement('button', props),
		PopupError: () => null,
	};
	if (name === './ScrollingText') return { ScrollingText: () => null };
	if (name === '../format') return { humanBytes: String };
	return require(name);
};
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Expand actual stateless popup components into their interactive host elements. */
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
		generation: 42, selected_field: 'Bucket', phase: 'Review', animation_frame: 0,
		uploaded_bytes: 0, total_bytes: null, validation_error: null, result_location: null,
		video_available: true,
		draft: { region: 'us-east-1', bucket: 'fixture-bucket', object_key: 'audio/fixture.opus', profile: '', upload_video: false },
	};
}

test('S3 review shows exact destination and only explicitly submits the live generation', () => {
	assert.equal(typeof module.exports.S3UploadPopup, 'function');
	for (const generation of [7, 42]) {
		const tree = module.exports.S3UploadPopup({ popup: { ...fixture(), generation } });
		const buttons = nodes(tree).filter((node) => node.type === 'button');
		buttons.find((node) => text(node) === 'Upload').props.onClick();
		assert.deepEqual(actions.pop(), [{ SubmitS3Upload: generation }]);
		assert.ok(text(tree).includes('s3://fixture-bucket/audio/fixture.opus'));
		assert.ok(text(tree).includes('Bucket permissions apply'));
		assert.ok(text(tree).includes('Existing objects are not overwritten'));
		assert.ok(text(tree).includes('AWS_PROFILE'));
		assert.ok(!text(tree).includes('public link'));
		buttons.find((node) => text(node) === 'Session keys…').props.onClick();
		assert.deepEqual(actions.pop(), ['OpenS3Credentials']);
		for (const field of ['Bucket', 'Region', 'ObjectKey', 'Profile']) {
			buttons.find((node) => node.props['data-s3-field'] === field).props.onClick();
			assert.deepEqual(actions.pop(), [{ SelectS3UploadField: field }]);
		}
	}
});

test('S3 busy and terminal states do not offer a retry and video respects capability', () => {
	assert.equal(typeof module.exports.S3UploadPopup, 'function');
	const review = module.exports.S3UploadPopup({ popup: { ...fixture(), video_available: false } });
	assert.ok(nodes(review).find((node) => node.props.role === 'checkbox').props.disabled);
	for (const phase of ['Preparing', 'Uploading', 'Cancelling', 'Complete', 'Failed', 'Cancelled']) {
		const tree = module.exports.S3UploadPopup({ popup: {
			...fixture(), phase, uploaded_bytes: 50, total_bytes: 100,
			result_location: phase === 'Complete' ? 's3://fixture-bucket/audio/fixture.opus' : null,
		} });
		const buttons = nodes(tree).filter((node) => node.type === 'button');
		assert.ok(!buttons.some((node) => ['Upload', 'Session keys…'].includes(text(node))));
		assert.ok(buttons.filter((node) => node.props['data-s3-field']).every((node) => node.props.disabled));
		assert.ok(!buttons.some((node) => text(node).includes('Open')));
		if (phase === 'Uploading') assert.ok(text(tree).includes('50%'));
		if (phase === 'Complete') assert.ok(text(tree).includes('s3://fixture-bucket/audio/fixture.opus'));
	}
});

test('S3 credentials project only three masked field lengths and return to review explicitly', () => {
	assert.equal(typeof module.exports.S3CredentialsPopup, 'function');
	const tree = module.exports.S3CredentialsPopup({ editor: {
		access_key_length: 12, secret_key_length: 18, session_token_length: 20,
		selected_field: 'SessionToken', validation_failed: true,
	} });
	const buttons = nodes(tree).filter((node) => node.type === 'button');
	for (const [label, field] of [['Access key', 'AccessKey'], ['Secret key', 'SecretKey'], ['Session token', 'SessionToken']]) {
		buttons.find((node) => text(node).includes(label)).props.onClick();
		assert.deepEqual(actions.pop(), [{ SelectS3CredentialField: field }]);
	}
	assert.ok(text(tree).includes('not saved'));
	assert.ok(text(tree).includes('optional'));
	buttons.find((node) => text(node) === 'Use for session').props.onClick();
	assert.deepEqual(actions.pop(), ['SubmitS3Credentials']);
});

test('S3 controls are feature-gated and mounted with a length-only credential contract', async () => {
	const contract = await readFile(new URL('./contract.ts', import.meta.url), 'utf8');
	const app = await readFile(new URL('./App.tsx', import.meta.url), 'utf8');
	const details = await readFile(new URL('./components/Details.tsx', import.meta.url), 'utf8');
	const fields = contract.match(/export interface S3CredentialsEditorView \{([^}]+)\}/)?.[1];
	assert.ok(fields);
	assert.deepEqual(fields.trim().split('\n').map((line) => line.trim().split(':')[0]), [
		'access_key_length', 'secret_key_length', 'session_token_length', 'selected_field', 'validation_failed',
	]);
	assert.match(app, /<S3UploadPopup popup=\{view\.s3_upload_popup\}/);
	assert.match(app, /<S3CredentialsPopup editor=\{view\.s3_credentials_editor\}/);
	assert.ok(details.includes('view.s3_upload_supported && view.s3_upload_available'));
	assert.ok(details.indexOf('>Upload to archive.org</Action>') < details.indexOf('>Upload to S3</Action>'));
	assert.ok(source.includes("['M', 'upload selected media to S3']"));
});
