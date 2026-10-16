/** Shared Artist controls use exact reducer metadata, not display-name searches. */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const actions = [];
const source = readFileSync(new URL('./components/Details.tsx', import.meta.url), 'utf8');
const compiled = ts.transpileModule(source, {
	compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const module = { exports: {} };
const mockRequire = (name) => {
	if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }) };
	if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
	if (name === '../searchHighlights') return { highlightRanges: () => [] };
	if (name === './SearchHighlight') return { SearchHighlight: ({ text }) => text };
	if (name.startsWith('.')) return new Proxy({}, { get: () => () => null });
	return require(name);
};
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Evaluate actual action components without mounting network-backed artwork. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Extract the visible hotkey and button label from actual JSX. */
function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

const render = (screen, available) => module.exports.Details({ kind: screen === 'YandexMusic' ? 'YandexMusic' : 'Generic', view: {
	screen, selected_artist_available: available, playlist_item: null, custom_command_buttons: [],
	yandex_music_actions: { artist_available: available },
	details: { title: 'Track', source: screen, media_id: { source: screen === 'SoundCloud' ? 'sound-cloud' : screen === 'Funkwhale' ? 'funkwhale' : 'yandex-music', external_id: 'track-1' },
		channel_id: '', playlist_names: [], links: [], description: '', timecodes: [], video_links: [], wikidata_entities: [], dearrow_title: null },
} });

test('SoundCloud and Yandex retain the shared standalone Artist action', () => {
	for (const screen of ['SoundCloud', 'YandexMusic']) {
		actions.length = 0;
		const artists = nodes(render(screen, true)).filter((node) => node.type === 'button' && text(node).includes('Artist'));
		assert.equal(artists.length, 1, screen);
		assert.equal(text(artists[0]), '[g] Artist', screen);
		assert.deepEqual(actions, [], 'rendering cannot load an artist');
		artists[0].props.onClick();
		assert.deepEqual(actions, ['OpenSelectedArtist']);
	}
});

/** Funkwhale keeps catalogue navigation beside its canonical artist and album links. */
const funkwhaleRail = (externalOpenerAvailable = true, capabilities = {}, albumDirectory = false) => module.exports.Details({ kind: 'Generic', view: {
	screen: 'Funkwhale', selected_artist_available: true, external_opener_available: externalOpenerAvailable,
	playlist_item: null, custom_command_buttons: [], yandex_music_actions: {},
	...capabilities,
	details: { title: 'Track', source: 'Funkwhale', media_id: albumDirectory ? null : { source: 'funkwhale', external_id: 'track-1' },
		channel_id: '', channel_name: 'Sonoridad', playlist_names: [], description: '', timecodes: [], video_links: [], wikidata_entities: [], dearrow_title: null,
		links: [
			{ prefix: '', label: 'Artist', url: 'https://audio.example/library/artists/42', presentation: 'LabelAndUrl',
				internal_target: { FunkwhaleArtist: 'https://audio.example/library/artists/42' }, wikidata_item_id: null },
			{ prefix: '', label: 'Album name', url: 'https://audio.example/library/albums/24', presentation: 'LabelAndUrl',
				internal_target: { FunkwhaleAlbum: 'https://audio.example/library/albums/24' }, wikidata_item_id: null },
		] },
} });

test('Funkwhale merges Artist and Album catalogue actions into their source-link lines', () => {
	const tree = funkwhaleRail();
	const buttons = nodes(tree).filter((node) => node.type === 'button');
	assert.equal(buttons.some((node) => text(node) === '[g] Artist'), false, 'no duplicate standalone action');
	const artist = buttons.find((node) => text(node) === 'Artist [g]: Sonoridad');
	const album = buttons.find((node) => text(node) === 'Album [b] Album name');
	assert.ok(artist);
	assert.ok(album);
	assert.equal(buttons.some((node) => text(node) === '↪'), false, 'inline hotkeys replace redundant internal markers');
	actions.length = 0;
	artist.props.onClick();
	album.props.onClick();
	assert.deepEqual(actions, ['OpenSelectedArtist', { OpenFunkwhaleAlbum: 'https://audio.example/library/albums/24' }]);
	for (const [index, url] of ['https://audio.example/library/artists/42', 'https://audio.example/library/albums/24'].entries()) {
		const link = buttons.find((node) => text(node) === url);
		assert.ok(link, 'the canonical HTTP URL remains visible and opens its indexed catalogue');
		actions.length = 0;
		link.props.onFocus();
		link.props.onClick();
		assert.deepEqual(actions, [{ SelectDetailLink: index }, { ActivateDetailLink: index }]);
	}
});

test('Funkwhale retains Commons and Archive upload buttons when core capabilities allow them', () => {
	const buttons = nodes(funkwhaleRail(true, {
		commons_upload_available: true, archive_upload_supported: true, archive_upload_available: true,
	})).filter((node) => node.type === 'button');
	actions.length = 0;
	buttons.find((node) => text(node) === '[U] To Commons').props.onClick();
	buttons.find((node) => text(node) === '[I] To archive.org').props.onClick();
	assert.deepEqual(actions, ['OpenCommonsUpload', 'OpenArchiveUpload']);
});

/** VK retains public URL order and gives each verified catalogue identity its own action. */
test('VK artist and album links navigate internally while the original stays a bare first URL', () => {
	const original = 'https://vk.ru/audio7_11';
	const album = 'https://vk.ru/music/album/-2000123_456';
	const artist = 'https://vk.ru/artist/fixture';
	const tree = module.exports.Details({ kind: 'Generic', view: {
		screen: 'Vk', selected_artist_available: true, external_opener_available: false,
		playlist_item: null, custom_command_buttons: [], yandex_music_actions: {},
		commons_upload_available: true, archive_upload_supported: true, archive_upload_available: true,
		details: { title: 'Track', source: 'VK', media_id: { source: 'vk', external_id: '7_11' },
			channel_id: '', channel_name: 'Fixture artist and collaborator', playlist_names: [], description: '',
			timecodes: [], video_links: [], wikidata_entities: [], dearrow_title: null, local_fingerprint_available: true,
			links: [
				{ prefix: '', label: 'VK original', url: original, presentation: 'UrlOnly', internal_target: null, wikidata_item_id: null },
				{ prefix: '', label: 'Fixture album', url: album, presentation: 'LabelAndUrl', internal_target: { VkAlbum: album }, wikidata_item_id: null },
				{ prefix: '', label: 'Fixture artist', url: artist, presentation: 'LabelAndUrl', internal_target: { VkArtist: artist }, wikidata_item_id: null },
			],
		},
	} });
	const all = nodes(tree);
	const buttons = all.filter((node) => node.type === 'button');
	assert.equal(text(all.find((node) => node.type === 'li')), original);
	assert.equal(text(tree).includes('VK original'), false);
	assert.equal(buttons.some((node) => text(node) === '[g] Artist'), false);
	assert.equal(buttons.some((node) => text(node) === '↪'), false);
	for (const [label, action] of [
		['Album [b] Fixture album', { OpenVkAlbum: album }],
		['Artist [g]: Fixture artist', { OpenVkArtist: artist }],
		['[U] To Commons', 'OpenCommonsUpload'],
		['[I] To archive.org', 'OpenArchiveUpload'],
		['[f] AcoustID', 'FingerprintLocalAudio'],
	]) {
		const button = buttons.find((node) => text(node) === label);
		assert.ok(button, label);
		assert.notEqual(button.props.disabled, true);
		actions.length = 0;
		button.props.onClick();
		assert.deepEqual(actions, [action]);
	}
	for (const [index, url] of [[1, album], [2, artist]]) {
		const button = buttons.find((node) => text(node) === url);
		assert.ok(button, url);
		assert.notEqual(button.props.disabled, true);
		actions.length = 0;
		const events = [];
		button.props.onKeyDown({ key: 'Enter', preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') });
		assert.deepEqual(events, ['prevent', 'stop']);
		assert.deepEqual(actions, [{ ActivateDetailLink: index }]);
	}
});

/** Album folders use Enter; b belongs to a selected track's album metadata. */
test('Funkwhale album directory advertises its working Enter shortcut', () => {
	const buttons = nodes(funkwhaleRail(false, {}, true)).filter((node) => node.type === 'button');
	assert.equal(buttons.some((node) => text(node).includes('Album [b]')), false);
	const album = buttons.find((node) => text(node) === 'Album [Enter] Album name');
	assert.ok(album);
	actions.length = 0;
	album.props.onClick();
	assert.deepEqual(actions, [{ OpenFunkwhaleAlbum: 'https://audio.example/library/albums/24' }]);
});

test('Funkwhale inline catalogue buttons work without an external opener and own keyboard activation', () => {
	const buttons = nodes(funkwhaleRail(false)).filter((node) => node.type === 'button');
	for (const [label, action] of [
		['Artist [g]: Sonoridad', 'OpenSelectedArtist'],
		['Album [b] Album name', { OpenFunkwhaleAlbum: 'https://audio.example/library/albums/24' }],
	]) {
		const button = buttons.find((node) => text(node) === label);
		assert.ok(button);
		assert.notEqual(button.props.disabled, true);
		for (const key of ['Enter', ' ']) {
			actions.length = 0;
			const events = [];
			button.props.onKeyDown({ key, preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') });
			assert.deepEqual(events, ['prevent', 'stop']);
			assert.deepEqual(actions, [action]);
			for (const modifiers of [{ repeat: true }, { altKey: true }, { ctrlKey: true }, { metaKey: true }, { shiftKey: true }]) {
				actions.length = 0;
				button.props.onKeyDown({ key, ...modifiers, preventDefault() {}, stopPropagation() {} });
				assert.deepEqual(actions, [], 'held or modified keys must not repeat catalogue navigation');
			}
		}
	}
	for (const [index, link] of buttons.filter((node) => text(node).startsWith('https://')).entries()) {
		assert.notEqual(link.props.disabled, true, 'HTTP catalogue links also work inside Youta without a browser');
		actions.length = 0;
		link.props.onClick();
		assert.deepEqual(actions, [{ ActivateDetailLink: index }]);
		actions.length = 0;
		const events = [];
		link.props.onKeyDown({ key: 'Enter', preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') });
		assert.deepEqual(events, ['prevent', 'stop'], 'a focused canonical URL must not start track playback');
		assert.deepEqual(actions, [{ ActivateDetailLink: index }]);
	}
});

test('an unknown artist identity never creates an Artist button', () => {
	for (const screen of ['SoundCloud', 'Funkwhale', 'YandexMusic']) {
		assert.equal(nodes(render(screen, false)).some((node) => node.type === 'button' && text(node).includes('Artist')), false, screen);
	}
});

/** No selected Details owner leaves an empty panel, even with stale action capabilities. */
test('empty Details omit the generic selection hint and stale controls', () => {
	for (const screen of ['Search', 'YouTubeMusic', 'SoundCloud', 'Funkwhale', 'ArchiveOrg', 'YandexMusic', 'Local']) {
		actions.length = 0;
		const tree = module.exports.Details({ kind: 'Generic', view: {
			screen, details: null, selected_artist_available: true, private_note_available: true,
			commons_upload_available: true, archive_upload_available: true,
		} });
		assert.equal(text(tree), '', screen);
		assert.equal(nodes(tree).some((node) => node.type === 'button'), false, screen);
		assert.deepEqual(actions, [], 'an unselected panel cannot request metadata or perform actions');
	}
});

/** Web retains its specific input guidance rather than losing all empty-state messages. */
test('empty Web Details retain useful directory guidance', () => {
	const tree = module.exports.Details({ kind: 'Generic', view: {
		screen: 'Web', details: null, search_query: '', search_editing: false, search_activity: null,
	} });
	assert.ok(text(tree).includes('Open an HTTP or HTTPS directory URL'));
	assert.equal(nodes(tree).some((node) => node.type === 'button'), false);
});
