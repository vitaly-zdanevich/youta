/** Funkwhale metadata reuses typed inline links and honest source-specific dates. */
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const modules = new Map();
const actions = [];

/** Execute actual metadata JSX with only hooks, artwork and native IPC stubbed. */
function load(url) {
	if (modules.has(url.href)) return modules.get(url.href);
	const compiled = ts.transpileModule(readFileSync(url, 'utf8'), {
		compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const module = { exports: {} };
	const mockRequire = (name) => {
		if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }) };
		if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
		if (name === './Artwork') return { Artwork: () => null };
		if (name === './CustomCommands') return { CustomCommandButtons: () => null };
		if (name.startsWith('.')) {
			const path = new URL(`${name}.ts`, url);
			return load(existsSync(path) ? path : new URL(`${name}.tsx`, url));
		}
		return require(name);
	};
	new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);
	modules.set(url.href, module.exports);
	return module.exports;
}

/** Expand real components while preserving browser host elements and callbacks. */
function nodes(element) {
	if (Array.isArray(element)) return element.flatMap(nodes);
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Read exactly what a user sees, without interpreting provider markup. */
function text(element) {
	if (Array.isArray(element)) return element.map(text).join('');
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

const { Description } = load(new URL('./components/Description.tsx', import.meta.url));
const { Details } = load(new URL('./components/Details.tsx', import.meta.url));

/** Exercise the real artwork component while retaining hook state between renders. */
function artworkRenderer() {
	const state = [];
	const refs = [];
	let stateIndex = 0;
	let refIndex = 0;
	const hooks = {
		useEffect: () => {},
		useRef(initial) {
			const index = refIndex++;
			if (!(index in refs)) refs[index] = { current: initial };
			return refs[index];
		},
		useState(initial) {
			const index = stateIndex++;
			if (!(index in state)) state[index] = initial;
			return [state[index], (value) => { state[index] = typeof value === 'function' ? value(state[index]) : value; }];
		},
	};
	const compiled = ts.transpileModule(readFileSync(new URL('./components/Artwork.tsx', import.meta.url), 'utf8'), {
		compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const module = { exports: {} };
	new Function('require', 'module', 'exports', compiled)((name) => {
		if (name === 'react') return { ...React, ...hooks };
		if (name === '../ipc') return { artworkSource: (url) => url ? `guarded:${url}` : null };
		return require(name);
	}, module, module.exports);
	return (props) => {
		stateIndex = 0;
		refIndex = 0;
		return module.exports.Artwork({ className: 'preview', ...props });
	};
}

/** Missing pod crops and error-XML originals must not hide valid album artwork. */
test('Funkwhale artwork retries advertised alternatives lazily through the guarded backend', () => {
	const render = artworkRenderer();
	const props = { url: 'https://audio.example/track-large.jpg',
		fallbackUrls: ['https://audio.example/track-original.png', 'https://audio.example/album-large.jpg'],
		unavailableLabel: 'Artwork unavailable on this pod' };
	let image = render(props);
	assert.equal(image.type, 'img');
	assert.equal(image.props.src, `guarded:${props.url}`);
	image.props.onError();
	image = render(props);
	assert.equal(image.type, 'img');
	assert.equal(image.props.src, `guarded:${props.fallbackUrls[0]}`);
	image.props.onError();
	image = render(props);
	assert.equal(image.type, 'img');
	assert.equal(image.props.src, `guarded:${props.fallbackUrls[1]}`);
	image.props.onLoad();
	assert.equal(render(props).props.src, `guarded:${props.fallbackUrls[1]}`);
	image.props.onError();
	const warning = render(props);
	assert.equal(warning.type, 'div');
	assert.equal(text(warning), 'Artwork unavailable on this pod');
	assert.notEqual(warning.props['aria-hidden'], true);
	const next = { ...props, url: 'https://audio.example/new-track.jpg' };
	assert.equal(render(next).props.src, `guarded:${next.url}`, 'a new owner resets exhausted artwork');
	image.props.onError();
	assert.equal(render(next).props.src, `guarded:${next.url}`, 'a retired image cannot fail its replacement');
});

/** Artwork fallback keeps first-working-track preference and a fixed attempt bound. */
test('Funkwhale artwork keeps working track images and deduplicates bounded alternatives', () => {
	const render = artworkRenderer();
	const props = { url: 'https://audio.example/primary.jpg',
		fallbackUrls: ['https://audio.example/primary.jpg', ...Array.from({ length: 12 }, (_, index) => `https://audio.example/album-${index}.jpg`)] };
	let image = render(props);
	image.props.onLoad();
	assert.equal(render(props).props.src, `guarded:${props.url}`);
	const sources = [];
	while (image.type === 'img') {
		sources.push(image.props.src);
		image.props.onError();
		image = render(props);
	}
	assert.equal(new Set(sources).size, sources.length);
	assert.ok(sources.length <= 9, 'at most the primary and eight alternatives');
});

/** Details must use the bounded preview, while retaining the album-fallback contract. */
test('Funkwhale Details forward advertised artwork alternatives without loading originals first', () => {
	const fallbacks = ['https://audio.example/track-original.png', 'https://audio.example/album-large.jpg'];
	const view = {
		screen: 'Funkwhale', playlist_item: null, custom_command_buttons: [], yandex_music_actions: {},
		details: { source: 'Funkwhale', title: 'Track', media_id: { source: 'funkwhale', external_id: 'track' },
			channel_id: '', playlist_names: [], links: [], description: '', timecodes: [], video_links: [], wikidata_entities: [], dearrow_title: null,
			thumbnail_url: 'https://audio.example/track-large.jpg', expanded_thumbnail_url: fallbacks[0], thumbnail_fallback_urls: fallbacks },
	};
	const tree = Details({ kind: 'Generic', view });
	const artwork = React.Children.toArray(tree.props.children)[0];
	assert.equal(artwork.props.url, view.details.thumbnail_url);
	assert.deepEqual(artwork.props.fallbackUrls, fallbacks);
	assert.equal(artwork.props.unavailableLabel, 'Artwork unavailable on this pod');
	assert.equal(artwork.props.prefetchUrl, null, 'only selected visible candidates load');
});
const tags = ['рок', 'field recording', '<script>literal</script>'];
const description = `🎵 Track notes\n\nTags: ${tags.join(', ')}`;
const links = tags.map((tag) => {
	const start_byte = new TextEncoder().encode(description.slice(0, description.indexOf(tag))).length;
	return { label: tag, url: '', prefix: '', presentation: 'LabelOnly', wikidata_item_id: null,
		internal_target: { FunkwhaleTag: tag }, description_range: { start_byte, end_byte: start_byte + new TextEncoder().encode(tag).length } };
});

test('Funkwhale tags retain UTF-8 text and use indexed internal actions without a browser', () => {
	actions.length = 0;
	const tree = Description({ text: description, links, timecodes: [], videoLinks: [], mediaId: null, externalOpenerAvailable: false });
	assert.equal(text(tree), description);
	const buttons = nodes(tree).filter((node) => node.type === 'button');
	assert.deepEqual(buttons.map(text), tags);
	assert.equal(nodes(tree).some((node) => node.type === 'script' || node.type === 'a'), false);
	assert.deepEqual(actions, [], 'rendering cannot search');
	buttons[1].props.onFocus();
	buttons[1].props.onClick();
	assert.deepEqual(actions, [{ SelectDetailLink: 1 }, { ActivateDetailLink: 1 }]);
	for (const key of ['Enter', ' ']) {
		actions.length = 0;
		const events = [];
		buttons[1].props.onKeyDown({ key, preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') });
		assert.deepEqual(events, ['prevent', 'stop']);
		assert.deepEqual(actions, [{ ActivateDetailLink: 1 }]);
	}
});

/** The uploader stays beside its URL and keyboard activation cannot start playback. */
test('Funkwhale uploader metadata is an internal, keyboard-owned button without a browser', () => {
	const label = 'Uploaded by [e]: emil@audio.example';
	const body = `${label}\nhttps://audio.example/federation/actors/emil`;
	const tree = Description({ text: body, links: [{
		label, url: 'https://audio.example/federation/actors/emil', prefix: '',
		presentation: 'LabelOnly', wikidata_item_id: null,
		internal_target: { FunkwhaleUploader: 'emil@audio.example' },
		description_range: { start_byte: 0, end_byte: label.length },
	}], timecodes: [], videoLinks: [], mediaId: null, externalOpenerAvailable: false });
	assert.equal(text(tree), body);
	const button = nodes(tree).find((node) => node.type === 'button' && text(node) === label);
	assert.ok(button);
	assert.notEqual(button.props.disabled, true);
	actions.length = 0;
	button.props.onClick();
	assert.deepEqual(actions, [{ ActivateDetailLink: 0 }]);
	for (const key of ['Enter', ' ']) {
		actions.length = 0;
		const events = [];
		button.props.onKeyDown({ key, preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') });
		assert.deepEqual(events, ['prevent', 'stop']);
		assert.deepEqual(actions, [{ ActivateDetailLink: 0 }]);
		for (const modifiers of [{ repeat: true }, { ctrlKey: true }, { altKey: true }, { metaKey: true }, { shiftKey: true }]) {
			actions.length = 0;
			button.props.onKeyDown({ key, ...modifiers, preventDefault() {}, stopPropagation() {} });
			assert.deepEqual(actions, []);
		}
	}
});

/** An added-to-pod timestamp is not necessarily the original release or upload date. */
test('Funkwhale Details show duration and the accurately labelled pod-added date', () => {
	const view = {
		screen: 'Funkwhale', playlist_item: null, custom_command_buttons: [], yandex_music_actions: {},
		details: { source: 'Funkwhale', title: 'Track', media_id: { source: 'funkwhale', external_id: 'track' },
			length: '2:03', published: '2024 April 13', channel_id: '', playlist_names: [], links: [],
			description: '', timecodes: [], video_links: [], wikidata_entities: [], dearrow_title: null },
	};
	let labels = nodes(Details({ kind: 'Generic', view })).filter((node) => node.type === 'dt').map(text);
	assert.ok(labels.includes('Duration'));
	assert.ok(labels.includes('Added to pod'));
	assert.equal(labels.includes('Published') || labels.includes('Uploaded'), false);
	assert.ok(text(Details({ kind: 'Generic', view })).includes('2024 April 13'));
	view.details.length = '';
	view.details.published = '';
	labels = nodes(Details({ kind: 'Generic', view })).filter((node) => node.type === 'dt').map(text);
	assert.equal(labels.includes('Duration') || labels.includes('Added to pod'), false);
});

/** License facts and linked licenses share one source-owned Details fixture. */
function licenseView(license, links = []) {
	return {
		screen: 'Funkwhale', playlist_item: null, custom_command_buttons: [], yandex_music_actions: {},
		details: { source: 'Funkwhale', title: 'Track', media_id: { source: 'funkwhale', external_id: 'track' },
			channel_id: '', playlist_names: [], links, license,
			description: '', timecodes: [], video_links: [], wikidata_entities: [], dearrow_title: null },
	};
}

/** A canonical license link replaces, rather than duplicates, the top license fact. */
test('Funkwhale linked license appears once with a readable label and canonical URL', () => {
	actions.length = 0;
	const url = 'https://creativecommons.org/licenses/by/4.0/';
	const view = licenseView('CC BY 4.0', [{ prefix: 'License: ', label: 'CC BY 4.0', url,
		presentation: 'LabelAndUrl', wikidata_item_id: null, internal_target: null, description_range: null }]);
	const tree = Details({ kind: 'Generic', view });
	const rendered = text(tree);
	assert.equal(rendered.match(/CC BY 4\.0/g)?.length, 1);
	assert.equal(rendered.split(url).length - 1, 1);
	assert.equal(nodes(tree).filter((node) => node.type === 'dt' && text(node) === 'License').length, 0);
	const buttons = nodes(tree).filter((node) => node.type === 'button' && text(node) === 'CC BY 4.0');
	assert.equal(buttons.length, 1);
	assert.equal(buttons[0].props.title, url);
	assert.deepEqual(actions, [], 'rendering cannot open the license page');
	buttons[0].props.onClick();
	assert.deepEqual(actions, [{ ActivateDetailLink: 0 }]);
});

/** Unrecognized rights notices remain plain text without invented links or permissions. */
test('Funkwhale unknown license remains a single plain-text fact', () => {
	actions.length = 0;
	const raw = 'custom-license-2026';
	const tree = Details({ kind: 'Generic', view: licenseView(raw) });
	assert.equal(nodes(tree).filter((node) => node.type === 'dt' && text(node) === 'License').length, 1);
	assert.equal(text(tree).split(raw).length - 1, 1);
	assert.equal(nodes(tree).some((node) => node.type === 'button' && text(node) === raw), false);
	assert.deepEqual(actions, []);
});

/** No placeholder license is added when the pod does not provide rights metadata. */
test('Funkwhale missing license is omitted', () => {
	for (const license of ['', null, undefined]) {
		const tree = Details({ kind: 'Generic', view: licenseView(license) });
		assert.equal(nodes(tree).some((node) => node.type === 'dt' && text(node) === 'License'), false);
		assert.equal(text(tree).includes('License:'), false);
	}
});
